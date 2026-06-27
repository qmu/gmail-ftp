-- System DB — migration #4 (roadmap M1, ticket t46): server-side sessions for the local web /
-- dashboard face. A session binds an HTTP request to a `users` row from t45 (AUTHENTICATION STATE,
-- decision §4.1 / §4.1) — it proves WHO, never WHAT-may-you-do (authorization is M2). Minting a
-- session is reversible infrastructure; an attached session is INERT until policy/OAuth land, so no
-- path silently trusts it this milestone.
--
-- APPEND-ONLY: migrations #1 (system.sql), #2 (system_audit.sql) and #3 (system_identity.sql) are
-- FROZEN — the checksum guard forbids editing a shipped migration in place; this table ships as a
-- NEW version (#4). The rusqlite `SessionStore` impl that FILLS these columns lives in the
-- binary-injected `session_store.rs` (qfs-store owns the connection); this migration only declares
-- the shape.

-- One server-side session. KEYED BY A HASH OF THE TOKEN, never the plaintext token: the cookie
-- carries the opaque high-entropy token; the DB stores only `sha256_hex(token)` (token hygiene,
-- RFD §10) — a System-DB leak therefore yields no usable session tokens (sha256 is preimage-
-- resistant). `user_id` is the authenticated human (t45 `users`); `expires_at` is the absolute
-- expiry checked on every lookup (an expired row is treated as absent and lazily reaped);
-- `rotated_from` records the PRIOR session's `token_hash` when this row was minted by a rotation
-- (sign-in / consent), preserving an audit breadcrumb without keeping the old token usable.
CREATE TABLE IF NOT EXISTS sessions (
    token_hash   TEXT PRIMARY KEY,
    user_id      INTEGER NOT NULL REFERENCES users(id),
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    expires_at   TEXT NOT NULL,
    rotated_from TEXT
);

-- Reaping expired rows scans by `expires_at`; resolving a user's sessions (a future "sign out
-- everywhere") scans by `user_id`. Index both so neither is a table scan.
CREATE INDEX IF NOT EXISTS sessions_expires_at ON sessions(expires_at);
CREATE INDEX IF NOT EXISTS sessions_user_id ON sessions(user_id);
