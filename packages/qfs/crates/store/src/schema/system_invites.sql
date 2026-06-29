-- System DB — migration #8 (roadmap M5, ticket t55): invites + memberships — the JOINING half of
-- decision B (every deployment holds its own users/accounts, §4.2). A host operator mints a one-time,
-- expiring INVITE; the invitee REDEEMS it to create their local identity (t45 users/accounts) and a
-- MEMBERSHIP linking them to the host (and, later, a project). Identity ≠ authorization (§4.1): a
-- membership says "you belong here", never "you may touch X" — the ACL is t57.
--
-- APPEND-ONLY: migrations #1–#7 are FROZEN — the checksum guard forbids editing a shipped migration
-- in place; these tables ship as a NEW version (#8). The rusqlite `InviteStore` impl that FILLS these
-- columns lives in the binary-injected `invite_store.rs` (qfs-store owns the connection); this
-- migration only declares the shape.

-- One invitation. The one-time token (the "signup URL" secret) is stored ONLY as its `sha256`
-- digest in `token_hash` (token hygiene, RFD §10) — never the plaintext: a System-DB leak therefore
-- yields no usable invite (sha256 is preimage-resistant), exactly as `sessions.token_hash` (t46) and
-- `accounts.password_hash` (t45) store hashes, never live secrets. The lifecycle is DERIVED from the
-- timestamps (no status column to drift): `consumed_at` (redeemed, single-use), `revoked_at`
-- (revoked before use), and `expires_at` (the absolute expiry checked at redeem). `email` is the
-- optional invitee handle (the delivery target when mail is configured — a documented seam); `scope`
-- /`project`/`role` seed the membership the redeemer joins; `created_by` records the minting operator
-- for audit.
CREATE TABLE IF NOT EXISTS invites (
    id          INTEGER PRIMARY KEY,
    token_hash  TEXT NOT NULL UNIQUE,
    email       TEXT,
    scope       TEXT NOT NULL DEFAULT 'host',
    project     TEXT,
    role        TEXT NOT NULL DEFAULT 'member',
    created_by  INTEGER REFERENCES users(id),
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    expires_at  TEXT NOT NULL,
    consumed_at TEXT,
    revoked_at  TEXT
);

-- Redeeming reaps/inspects by expiry; index it so neither a sweep nor a lifecycle check is a table
-- scan. (`token_hash` is already indexed by its UNIQUE constraint — the redeem lookup key.)
CREATE INDEX IF NOT EXISTS invites_expires_at ON invites(expires_at);

-- One membership: the link that makes a t45 `users` row a MEMBER of the host (scope='host') or a
-- project (scope='project', `project` set). `role` is a coarse label for the LATER ACL (t57), NOT an
-- authorization grant (§4.1). `invite_id` records which invite created this membership (audit / no
-- orphan), NULL for a membership created by another path (e.g. the bootstrap owner).
CREATE TABLE IF NOT EXISTS memberships (
    id         INTEGER PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id),
    scope      TEXT NOT NULL DEFAULT 'host',
    project    TEXT,
    role       TEXT NOT NULL DEFAULT 'member',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    invite_id  INTEGER REFERENCES invites(id)
);

-- A user belongs to a given (scope, project) at most once — the uniqueness that makes "is a member"
-- unambiguous. `project` is NULL for a host membership; `COALESCE(project,'')` folds that NULL into a
-- single comparable value so two host memberships for the same user collide (a bare UNIQUE over a
-- NULL column would treat NULLs as distinct and permit duplicates).
CREATE UNIQUE INDEX IF NOT EXISTS memberships_user_scope_project
    ON memberships(user_id, scope, COALESCE(project, ''));

-- Resolving a user's memberships scans by `user_id`; index it.
CREATE INDEX IF NOT EXISTS memberships_user_id ON memberships(user_id);
