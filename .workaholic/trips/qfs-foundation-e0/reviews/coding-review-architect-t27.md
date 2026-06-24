# Coding Review (Architect) — t27 Credential / secret store + multi-account resolution

- Author: Architect (Neutral)
- Phase: Coding / review-and-testing
- Target: t27 — `qfs-secrets` crate + Engine `with_secrets` + `qfs account` stubs
- Commit: `72fce87` on `work-20260622-230954`
- Mode: analytical review only (no cargo/test execution)
- Files read: `crates/secrets/src/{secret,key,store,backends,active,resolve,local,worker,lib}.rs`,
  `crates/secrets/Cargo.toml`, `crates/core/src/lib.rs`, `crates/cmd/src/lib.rs`,
  `crates/cmd/tests/dep_direction.rs`, `ARCHITECTURE.md`, ticket.

## Decision

**Approve with minor suggestions.**

The headline redaction invariant is structurally sound: it is **not** achievable to leak a
`Secret` value through Debug / Display / serde / Clone / error / log without an explicit,
grep-able `.expose()` at the call site. Cross-driver isolation holds by construction.
Multi-account resolution is fail-closed and unambiguous. Backend security (AEAD + argon2id +
0600 + atomic write) is correct. The spine is a clean leaf (`qfs-secrets → qfs-types`) locked by
the dep-direction test, with a clean native/wasm cfg split. No security defect found — hence not
a revision request. The suggestions below are hardening / doc-fidelity items, not blockers.

---

## 1. Redaction invariant (headline) — PASS

**Structurally impossible to leak by accident.** Verified four independent ways:

1. **No leaking trait impls on `Secret`.** `secret.rs` implements only `Debug` (redacting,
   `Secret(***redacted***)`), `Display` (bare `***redacted***`), and `From<{&str,String,Vec<u8>}>`.
   There is **no** `Clone`, **no** `Serialize`/`Deserialize`, **no** `Deref`/`AsRef<str>`/
   `Into<String>`. So a `Secret` cannot be silently duplicated, serde-serialized into JSON / an
   audit record / a config file, or coerced into a formattable `&str`. A grep of `secret.rs`
   confirms no `clone|serialize|deserialize|deref` impl on the type.

2. **`expose()` is the only door and it is grep-able.** A workspace-wide `grep '\.expose('`
   returns exactly **three** non-test, non-doc call sites, every one of them justified:
   - `local.rs:103` — `passphrase.expose()` feeding argon2 KDF input (must hash the bytes).
   - `local.rs:200` — `value.expose().to_vec()` serializing the secret into the *cleartext map
     that is then AEAD-sealed* (never written in the clear).
   - `backends.rs:76` — `entry.value.expose().to_vec()` re-wrapping into a fresh `Secret` for
     the caller (because `Secret` is not `Clone` — the deliberate consequence).
   None of the three feeds `format!`/`tracing`/an error string. The CI grep guard the ticket
   asks for (reject `.expose(` near `format!`/`tracing`) is therefore tractable: the surface to
   police is tiny and stable.

3. **`Zeroizing` drop is sound for the live `Secret`.** `Secret(Zeroizing<Vec<u8>>)` — the
   `zeroize` crate zeroes the heap buffer on drop. `from_string` consumes the `String`'s buffer
   (no lingering plaintext copy in the caller); `new` takes ownership of the `Vec`. The `Entry`
   in `InMemoryStore` is itself **not** `Debug` and stores the `Secret` (not raw bytes), so the
   in-memory backend zeroizes too.

4. **No backend formats/logs plaintext.** Every `SecretError` variant is secret-free by
   construction: `NotFound(CredentialKey)` carries selectors only; `Locked` is a constant;
   `Backend(String)` is only ever constructed from operation descriptions ("reading credential
   blob", "atomic rename of credential blob", etc.) and serde/io error display — never from a
   `Secret` (which has no `Display`/`Into<String>` to feed it). `decrypt` deliberately maps a
   wrong-key/tamper failure to `Locked` **without echoing any bytes**. `dispatch_account` in
   `qfs-cmd` logs only the `feature` label via `tracing`, never a credential.

**Canary test coverage is genuine.** `lib.rs::a_planted_secret_never_appears_in_any_error_or_log_surface`
plants `PLANTED-LEAK-CANARY-…`, then asserts its absence (full string *and* a fragment) across
`Debug`/`Display` of the `Secret` plus `Debug`+`Display` of **every** error type that could
carry text — `NotFound`, `Backend`, `Locked`, `ScopeError`, and `Ambiguous` (the resolve path).
It also positively asserts the redaction marker *did* render. `secret.rs` adds the nested-in-
`#[derive(Debug)]` case (a `Secret` field inside a derived-Debug struct stays redacted), which is
the realistic accidental-leak vector. This is the right structural coverage.

> Minor (S1, hardening — not a leak): the **decrypted vault plaintext is not zeroized.**
> `local.rs`'s `StoredEntry.secret: Vec<u8>` and the `Vault` it lives in are plain `Vec<u8>`;
> after `load()`/`get()` returns, the decrypted secret bytes that transited the `Vault` linger in
> freed heap until overwritten. This does **not** breach the documented threat model (at-rest
> only; live-process-memory compromise is explicitly out of scope, local.rs §"Threat model"),
> and the *returned* `Secret` is zeroized. But the crate's own headline promise is "zeroized on
> drop," and the transient cleartext map is the one place that promise is partial. Suggest wrapping
> `StoredEntry.secret` in `Zeroizing<Vec<u8>>` (it is serde-serializable via the inner `Vec`) or
> dropping the `Vault` through a zeroizing buffer, so the in-flight decrypt is wiped too. Low
> priority; documentable as a known limitation if deferred.

## 2. Cross-driver isolation — PASS

`CredentialKey = (DriverId, AccountId)` makes cross-driver access **impossible by construction**,
not policed: a key *names* exactly one driver, and there is no API that returns "any driver's"
secret. `get/put/remove` take a `&CredentialKey`; `list(Option<&DriverId>)` filters to one driver
(or all, for the admin `qfs account list` surface). `resolve()` filters `available` to the passed
`driver` *first* (`resolve.rs:132` `.filter(|r| &r.driver == driver)`) and the caller doc requires
`available` already be driver-scoped — there is no resolve path that yields an account from another
driver. The `flat()` encoding (`driver/account`) is unambiguous because `AccountId::new` rejects
`/`, `@`, and whitespace, so the key cannot be spoofed across the separator. Sound.

## 3. Multi-account resolution — PASS (fail-closed)

Precedence `flag > at_clause > active > sole > error` is implemented exactly and is unambiguous:
the explicit-selector chain is an `or_else` ladder (flag, then AT, then active), and **a chosen
selector that does not name a configured account returns `UnknownSelection` — it does NOT fall
through to "sole"** (`resolve.rs:150-159`). That is the key fail-closed property: a typo'd
`--account` errors loudly with the candidate list rather than silently binding the sole account.
With no selector: `[] → NoneConfigured`, `[one] → Sole`, `[many] → Ambiguous{candidates}` — never
a silent pick on ambiguity. The slice-pattern (`[_only]`) avoids `unwrap`/`expect`, honoring the
no-panic lib policy. "Sole" is correctly *sole-for-this-driver* (the per-driver filter), verified
by `sole_account_for_driver_only`. The `AccountSource` recorded for audit is a `Copy` enum of
four selectors with stable string labels — secret-free, exactly the "who ran as whom" the audit
ledger needs, carrying no credential. Tests cover the full ladder, both error variants, and the
typo case.

## 4. Backend security — PASS

- **AEAD:** ChaCha20-Poly1305, 256-bit key, **fresh 96-bit random nonce per write** from
  `rand::rng()` (a `ThreadRng` CSPRNG; `fill_bytes` resolves via the `Rng`/`RngCore` trait,
  verified against the rand 0.10.1 source). On-disk form is `MAGIC || nonce || ciphertext`; the
  round-trip test asserts the plaintext token is **absent** from the raw blob. Wrong key / tamper
  → authenticated-decryption failure → `Locked`, no byte echo. Sound.
- **KDF:** argon2id (`Argon2::default()`) `hash_password_into` a 32-byte key from
  `passphrase.expose()` + caller-supplied salt. The salt is honestly caller-managed (doc says so);
  reproducible key for the no-keyring fallback.
- **Perms:** `0600` asserted three ways — created with `.mode(0o600)`, re-`set_permissions(0o600)`
  to defeat a leftover-temp inheriting old perms, and `verify_owner_only` (`mode & 0o077 != 0` →
  reject) on **every** `load()` (defense-in-depth against a post-create `chmod`). Parent dir
  created `0o700`. Tests cover created-mode-0600 and group-readable-rejected.
- **Atomic write:** temp (`.tmp`) → `write_all` → `fsync` (`sync_all`) → `rename` over target, with
  best-effort temp cleanup on rename failure. A crash before rename leaves the prior blob intact;
  `prior_blob_survives_a_dangling_temp` proves a stray garbage `.tmp` does not corrupt the live
  blob. No plaintext-at-rest, no temp-file plaintext leak (the temp holds ciphertext).
- **EnvStore/WorkerStore** are read-only (write/remove → structured `Backend` error, not silent
  drop). `EnvStore`'s reader/names are **injectable closures** — tests use `from_map` fixtures and
  never touch the global, racy process environment, avoiding env-var test races. Correct.

> Minor (S2, hardening): `verify_owner_only` checks the **file** mode but not whether the parent
> directory is world-writable, and `open_with_key` only verifies perms `if path.exists()` — a
> first-`put` creating the file is covered by `write_owner_only`'s 0600, so this is consistent, but
> a world-writable *containing directory* (where another user could swap the blob) is outside the
> current check. Out-of-scope for the documented at-rest model (an attacker who can write the dir is
> a host-level compromise), but worth a one-line note in the threat model that directory ACLs are
> the host's responsibility. No code change required.

## 5. Spine / wasm — PASS

`qfs-secrets` is a clean leaf: `Cargo.toml` declares exactly one workspace dep, `qfs-types`
(reusing the canonical `DriverId`), plus third-party `thiserror`/`serde`/`zeroize`/`time`, with
the AEAD/argon2/rand deps under a `cfg(not(wasm32))` target table so they are **compiled out** on
wasm. `lib.rs` cfg-splits `mod local` (`not(wasm32)`) vs `mod worker` (`wasm32`) and gates the
re-exports identically — a clean compile-time split, not a runtime skip. `dep_direction.rs`'s
`secrets_is_confined_to_types_and_core_consumes_it` mechanically locks (a) secrets → only
qfs-types among workspace crates and (b) qfs-core consumes secrets. The Engine threads
`Option<Arc<dyn Secrets>>` via `with_secrets`, object-safe (`Send + Sync`), oblivious to the
backend — so a `Plan` embeds only an account *selector*, never a secret (purity invariant intact).

> Minor (S3, doc fidelity): **`ARCHITECTURE.md` was not updated for `qfs-secrets`.** The Crate map
> table (line 13+) and the Dependency-spine block (line 30+) list every other crate but omit
> `crates/secrets` / `qfs-secrets` and the `qfs-secrets → qfs-types` + `qfs-core → qfs-secrets`
> edges. The dep-direction test *enforces* the edge, so the code is correct; but the durable
> companion doc is now stale. Suggest adding the row + the two edges so ARCHITECTURE.md stays the
> faithful map it claims to be ("Every later ticket must add code inside these boundaries"). Doc-
> only; no behavior impact.

## 6. E4 consumability + `qfs account` park honesty — PASS

The store + scope model is shaped to let E4 auth drivers (t19 Google OAuth multi-account, t22-t25)
land **without restructuring**: a driver fetches via `&dyn Secrets` keyed by its own
`(DriverId, AccountId)`; multi-account is already first-class (resolve ladder + `ActiveAccounts`);
`grant_scopes(required, held)` gives the OAuth-scope grant/deny over `requires_scopes` labels
(t13) returning a secret-free `ScopeGrant`/`ScopeError` — exactly the substrate a refresh-token
driver needs. `put` consumes a `Secret` by value, so a driver that mints/refreshes a token stores
it without a lingering copy. Nothing here forces a later epic to re-key or re-trait.

The `qfs account` park is **honest**: `cmd/lib.rs` declares the full structured `AccountVerb`
enum (`add|list|use|remove` with typed args), parses cleanly (tests assert `qfs account list/add`
parse and exit 1), and each verb dispatches to a structured, secret-free `NotImplemented` — the
same E0 stub pattern as `run`/`serve`/`shell`. The doc-comment is explicit that the
credential-bearing I/O (prompt → keyring/passphrase → encrypted backend) is the parked seam and
that **no credential is ever read from argv** (would leak into shell history / `ps`) — a correct
security stance to bake into the stub now. This is a genuine structured park, not a hollow one.

## Concern + proposal (per Critical Review Policy)

**Concern:** the crate's "zeroized on drop" headline is *partial* — the transient decrypted
`Vault`/`StoredEntry` plaintext in `LocalStore` is plain `Vec<u8>` (S1), so an in-flight secret
lingers in freed heap. It is within the documented at-rest-only threat model, but the gap is
between the promise's wording and the one code path that doesn't honor it.

**Proposal (structural, fidelity-preserving):** wrap `StoredEntry.secret` in
`zeroize::Zeroizing<Vec<u8>>` (serde-transparent through the inner `Vec`) so the decrypt-side
cleartext is wiped on `Vault` drop too — making the "zeroized" guarantee total and matching the
crate doc's wording. If deferred, add one sentence to `local.rs`'s threat-model block stating the
transient decrypt buffer is not zeroized (so the limitation is documented, not silent). Pair this
with the S3 ARCHITECTURE.md row so the durable map and the headline promise both stay faithful.

## Verdict

No redaction leak path exists; cross-driver isolation, fail-closed resolution, and backend
crypto/perms/atomicity are all correct. The three minor items (transient-plaintext zeroize, dir-
ACL threat-model note, ARCHITECTURE.md crate-map row) are hardening / doc-fidelity, not defects.
**Approve with minor suggestions.**
