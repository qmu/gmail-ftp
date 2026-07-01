-- Project DB — migration #8 (EPIC 20260701100000 / t100020 — the `CONNECT` defined-path model):
-- the DEFINED-PATH binding registry.
--
-- APPEND-ONLY: migrations #1..#7 are FROZEN (the checksum guard forbids editing a shipped
-- migration); this table ships as a NEW version.
--
-- A `CONNECT /<path> TO <driver> [AT '<loc>'] [SECRET '<ref>']` binds a user-chosen PATH to a
-- driver + credential — a "defined path" that MOUNTS a connection (the path IS the connection
-- identity). A `CONNECT /<path> TO /<existing-path>` is an ALIAS: a second path reusing the same
-- connection (`alias_of` points at the target defined path). `DISCONNECT /<path>` removes a row.
-- The project DB is the SINGLE SOURCE OF TRUTH — there is NO `connections.qfs` config file.
--
-- SELECTORS + METADATA ONLY — never a secret. `secret_ref` is a REFERENCE (`env:VAR` /
-- `vault:driver/connection`) resolved at USE time (`qfs::secret_ref::resolve_secret_ref`), never a
-- secret VALUE: an `env:` ref reads the environment at use (never persisted) and a `vault:` ref
-- points at the envelope-encrypted `secret_store` (migration #2). An unresolvable ref leaves the
-- path DEFINED but FAIL-CLOSED (reading it errors "not connected"), never a fake mount. Like
-- `active_account` / `shared_connection`, this carries no key material, so it needs no passphrase
-- to read (the registration + resolver consult it on the passphrase-free path).
CREATE TABLE IF NOT EXISTS path_binding (
    -- The user-defined path (the mount point), canonicalized as `/a/b/c`. One binding per path.
    path       TEXT PRIMARY KEY,
    -- The canonical driver id (the plan identity `Driver::id()`, e.g. `postgres`) for a FULL
    -- connect; NULL for an ALIAS row (an alias inherits its target's driver). Kept canonical so the
    -- `/<driver.id()>/<sub>` reconstruction keeps the per-driver path parsers working untouched.
    driver_id  TEXT,
    -- The non-secret connection locator (the `AT` clause, e.g. `postgres://db/orders`). NULL when
    -- the driver needs none / for an alias. A §10 hint, never a token.
    at_locator TEXT,
    -- The secret REFERENCE (`env:VAR` / `vault:driver/connection`), resolved at USE time. NULL for
    -- no credential / for an alias. NEVER a secret VALUE (the redaction contract, §3.2).
    secret_ref TEXT,
    -- For the ALIAS arm: the target defined `path` this row reuses. Mutually exclusive with
    -- `driver_id`. Removing the target removes its aliases (enforced in the binding I/O, and the FK
    -- cascades when `PRAGMA foreign_keys=ON`).
    alias_of   TEXT REFERENCES path_binding(path) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);
