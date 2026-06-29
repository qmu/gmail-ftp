-- Project DB — migration #1 (roadmap §4.2): per-PROJECT scope.
--
-- EMPTY SKELETON. t42 ships only these minimal placeholders; t43 (envelope-encrypted secrets),
-- t44 (`accounts`→`connections`), and the rest of M0–M3 fill the real columns in their OWN
-- migrations. Do NOT pre-build those columns here — keep each PR's schema delta reviewable.

-- This project's connections to external services (t43/t44 give it the real envelope columns).
CREATE TABLE IF NOT EXISTS connections (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);

-- Project-scoped configuration shell (typed later by t53).
CREATE TABLE IF NOT EXISTS project_config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Durable per-project state shell (cursors, watermarks, etc.; filled by later tickets).
CREATE TABLE IF NOT EXISTS project_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
