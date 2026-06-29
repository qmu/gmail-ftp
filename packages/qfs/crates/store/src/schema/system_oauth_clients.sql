-- System DB — migration #6 (roadmap M2, ticket t49): the OAuth authorization-server FLOW state —
-- dynamically-registered clients (RFC 7591), short-lived authorization codes (the auth-code + PKCE
-- grant), and the refresh-token handle skeleton. This is the live-handshake half of making a qfs
-- server its OWN authorization server (decision C, §4.1): t48 shipped discovery + signing keys; this
-- ticket issues the authorization code and exchanges it for a signed access token (t50 enforces the
-- token in front of the MCP endpoint).
--
-- APPEND-ONLY: migrations #1 (system.sql), #2 (system_audit.sql), #3 (system_identity.sql), #4
-- (system_sessions.sql) and #5 (system_oauth_keys.sql) are FROZEN — the checksum guard forbids
-- editing a shipped migration in place; this ships as a NEW version (#6). The rusqlite store that
-- FILLS these columns lives in the binary-injected `oauth_store.rs` (qfs-store owns the connection);
-- this migration only declares the shape.

-- One dynamically-registered OAuth client (RFC 7591). `client_id` is the minted public identifier.
-- `redirect_uris` is the EXACT allowlist a `redirect_uri` is matched against at /authorize and /token
-- (open-redirect defense — no wildcard/substring matching), stored as newline-separated absolute
-- URIs. `client_secret_hash` is NULL for the public PKCE clients an MCP client registers (no secret —
-- PKCE is the proof-of-possession); when present it is the `sha256_hex` of the secret, NEVER the raw
-- secret (secret hygiene, RFD §10). `client_name` is the optional human label from the request.
CREATE TABLE IF NOT EXISTS oauth_clients (
    client_id          TEXT PRIMARY KEY,
    redirect_uris      TEXT NOT NULL,
    client_name        TEXT,
    client_secret_hash TEXT,
    created_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);

-- One short-lived, single-use authorization code. KEYED BY A HASH OF THE CODE, never the plaintext:
-- the redirect carries the opaque high-entropy code; the DB stores only `sha256_hex(code)` (a leak of
-- the System DB therefore yields no usable codes — sha256 is preimage-resistant). The code is BOUND
-- to the exact `client_id` + `redirect_uri` + the PKCE `code_challenge`/`code_challenge_method`
-- (S256) + the authenticated `user_id`, all re-checked at the token endpoint. `expires_at` is the
-- absolute (short, ~60s) expiry checked on exchange (an expired row is treated as absent and reaped);
-- the row is DELETED on first exchange (single-use — a replay finds nothing and is rejected).
CREATE TABLE IF NOT EXISTS oauth_codes (
    code_hash           TEXT PRIMARY KEY,
    client_id           TEXT NOT NULL REFERENCES oauth_clients(client_id),
    user_id             INTEGER NOT NULL REFERENCES users(id),
    redirect_uri        TEXT NOT NULL,
    pkce_challenge      TEXT NOT NULL,
    pkce_method         TEXT NOT NULL,
    scope               TEXT NOT NULL DEFAULT '',
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    expires_at          TEXT NOT NULL
);

-- Exchanging/reaping a code scans by `expires_at`; index it so neither is a table scan.
CREATE INDEX IF NOT EXISTS oauth_codes_expires_at ON oauth_codes(expires_at);

-- One refresh-token handle (issued here at the token endpoint; ENFORCED/ROTATED in t50). KEYED BY A
-- HASH of the opaque handle, never the plaintext (same hygiene as the auth code / session token). It
-- carries the `user_id` + `client_id` + `scope` a refresh exchange will re-mint an access token for,
-- and `rotated_from` (the prior handle's hash) for rotation auditing once t50 wires the refresh grant.
CREATE TABLE IF NOT EXISTS oauth_refresh_tokens (
    handle_hash  TEXT PRIMARY KEY,
    user_id      INTEGER NOT NULL REFERENCES users(id),
    client_id    TEXT NOT NULL REFERENCES oauth_clients(client_id),
    scope        TEXT NOT NULL DEFAULT '',
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    expires_at   TEXT NOT NULL,
    rotated_from TEXT
);

-- Reaping expired handles / a future "revoke all for a user" scans by these; index them.
CREATE INDEX IF NOT EXISTS oauth_refresh_expires_at ON oauth_refresh_tokens(expires_at);
CREATE INDEX IF NOT EXISTS oauth_refresh_user_id ON oauth_refresh_tokens(user_id);
