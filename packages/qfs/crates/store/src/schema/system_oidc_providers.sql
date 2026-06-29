-- System DB — migration #9 (roadmap M5, ticket t56): upstream OIDC federation providers — the
-- "hub model" registration store. t48 made qfs its OWN authorization server (it MINTS tokens); t56
-- makes qfs ALSO a relying party (RP) that TRUSTS one or more UPSTREAM IdPs (qfs Cloud / Google
-- Workspace / Entra / Okta / a generic OIDC provider) for the human-login leg (decision D, §4.1).
-- This table records each registered upstream: its issuer, the client id qfs uses against it, the
-- envelope-encrypted client secret, and the cached discovery/JWKS so the verifier need not re-fetch
-- on every login. A row here only says "this host TRUSTS this issuer for authentication" — identity,
-- never authorization (§4.1; the ACL is t57).
--
-- APPEND-ONLY: migrations #1–#8 are FROZEN — the checksum guard forbids editing a shipped migration
-- in place; these tables ship as a NEW version (#9). The rusqlite store that FILLS these columns
-- lives in the binary-injected `oidc_provider_store.rs` (qfs-store owns the connection); this
-- migration only declares the shape.

-- One registered upstream OIDC provider. `provider` is the LOCAL key (an operator label like
-- 'google', or the issuer URL) — it is the `provider` half of an `accounts(provider, subject)` link
-- (qfs-identity), NOT the 'local' password provider. `client_id` is the RP client qfs presents to the
-- upstream. `client_secret_encrypted` is the upstream client secret SEALED under the System-DB
-- data-key (DEK) with a fresh per-row nonce prefix (`nonce || ciphertext`) — mirroring how
-- `oauth_keys.private_key_encrypted` (t48) and the Project DB's `secret_store` (t43) protect secrets;
-- the plaintext secret NEVER lands here, and it is NULL for a public client (PKCE-only, no secret).
-- `authorization_endpoint`/`token_endpoint`/`jwks_uri` + `jwks_json` cache the upstream's discovery
-- document (`.well-known/openid-configuration`) + its published JWKS so the verifier resolves the
-- signing key offline; they are refreshed by the binary's discovery fetch (a documented native seam).
-- `scopes` is the space-delimited scope set requested at the upstream (default OIDC `openid email
-- profile`). `redirect_uri` is OUR RP callback the upstream redirects back to.
CREATE TABLE IF NOT EXISTS oidc_providers (
    provider                TEXT PRIMARY KEY,
    issuer                  TEXT NOT NULL,
    client_id               TEXT NOT NULL,
    client_secret_encrypted BLOB,
    redirect_uri            TEXT,
    scopes                  TEXT NOT NULL DEFAULT 'openid email profile',
    authorization_endpoint  TEXT,
    token_endpoint          TEXT,
    jwks_uri                TEXT,
    jwks_json               TEXT,
    created_at              TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);

-- Resolving "which provider trusts this issuer" looks up by issuer; index it so the lookup is not a
-- scan. (`provider` is already indexed by its PRIMARY KEY.)
CREATE INDEX IF NOT EXISTS oidc_providers_issuer ON oidc_providers(issuer);

-- The single-row envelope metadata for the upstream-client-secret data-key: the once-generated DEK
-- wrapped under a passphrase-derived KEK (`wrapped_dek`) + the per-store argon2id `kdf_salt`. Mirrors
-- `oauth_key_meta` (t48) / the Project DB's `secret_meta` (t43) but scoped to the OIDC provider
-- secrets. The CHECK pins exactly one row. Unlocking is derive_kek(QFS_PASSPHRASE, kdf_salt) + unwrap,
-- at boot — so a System-DB leak yields no usable upstream client secret without the passphrase.
CREATE TABLE IF NOT EXISTS oidc_provider_meta (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    wrapped_dek BLOB NOT NULL,
    kdf_salt    BLOB NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);
