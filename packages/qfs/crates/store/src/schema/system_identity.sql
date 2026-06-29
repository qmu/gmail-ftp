-- System DB — migration #3 (roadmap M1, ticket t45): the identity tables — the first real notion of
-- WHO a human is. AUTHENTICATION ONLY (decision §4.1: identity is not authorization); no privilege
-- falls out of these rows yet (sessions are t46, real auth is M2).
--
-- APPEND-ONLY: migrations #1 (system.sql) and #2 (system_audit.sql) are FROZEN — the checksum guard
-- forbids editing a shipped migration in place; these tables ship as a NEW version (#3). The rusqlite
-- `IdentityStore` impl that FILLS these columns lives in the binary-injected `identity_store.rs`
-- (qfs-store owns the connection); this migration only declares the shape.

-- One human identity. `primary_email` is the unique human handle; `status` is a lifecycle marker that
-- gates nothing in t45 (default 'active'). Per-host (decision B / §4.2): every deployment holds its
-- own users.
CREATE TABLE IF NOT EXISTS users (
    id            INTEGER PRIMARY KEY,
    primary_email TEXT NOT NULL UNIQUE,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    status        TEXT NOT NULL DEFAULT 'active'
);

-- A linked sign-in identity for a user (many-to-one). `provider='local'` rows carry an argon2id
-- `password_hash` (PHC string); OAuth/OIDC providers (M2) leave it NULL and authenticate by subject.
-- This is the IDENTITY account — explicitly NOT the t44 credential connection (a stored service
-- token); the two share neither name nor table. `subject` is the provider-scoped identifier (the
-- email for a local account).
CREATE TABLE IF NOT EXISTS accounts (
    id            INTEGER PRIMARY KEY,
    user_id       INTEGER NOT NULL REFERENCES users(id),
    provider      TEXT NOT NULL,
    subject       TEXT NOT NULL,
    password_hash TEXT,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);

-- A given (provider, subject) names exactly one account — the uniqueness that makes a sign-in
-- identity unambiguous (and, for 'local', one account per email).
CREATE UNIQUE INDEX IF NOT EXISTS accounts_provider_subject ON accounts(provider, subject);
