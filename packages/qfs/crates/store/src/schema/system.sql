-- System DB — migration #1 (roadmap §4.2): per-HOST scope.
--
-- EMPTY SKELETON. t42 ships only the runner + these minimal table shells; later M0–M3 tickets fill
-- the columns in their OWN migrations so each PR's schema delta stays reviewable (do NOT pre-build
-- t43 secret / t45 identity columns here). Keep this genuinely minimal.

-- The host's projects (roadmap §4.2: the System DB lists projects; a Project DB holds each
-- project's own connections/config/state). `slug` is the stable external handle; `id` is internal.
CREATE TABLE IF NOT EXISTS projects (
    id         INTEGER PRIMARY KEY,
    slug       TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);

-- Cross-project host configuration as a typed-later key/value shell (t53 `/sys/*` gives it shape).
CREATE TABLE IF NOT EXISTS system_config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
