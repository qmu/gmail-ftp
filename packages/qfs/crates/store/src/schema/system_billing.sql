-- System DB — migration #12 (roadmap §3.4 / M9, ticket t67): the per-team BILLING PLAN the
-- `/sys/billing` admin path reads and (gated) writes, plus the webhook DEDUP ledger.
--
-- `/sys/billing` is the path façade over the billing-tier model (`qfs_identity::billing`): one row
-- per team — its `team_id`, the recorded `tier` (`free-individual` / `paid-team`), the subscription
-- `status` (`active` / `past-due` / `canceled` / `inactive`), and the `current_period_end` the
-- provider reported. Plan state is DATA: `FROM /sys/billing` reads it, a super-admin (or a provider
-- webhook) `INSERT INTO /sys/billing` upserts it on `team_id`, and the ENTITLEMENT GATE reads it to
-- decide whether a paid-only capability (team-wide brokered connections, t66/t81) is permitted —
-- default-deny toward the free tier (a missing/unknown/lapsed plan resolves to free). Every mutation
-- also appends a t76 audit row (administration observes itself).
--
-- SECRETS NEVER LAND HERE (roadmap §3.2 redaction): the payment provider's API keys and the webhook
-- signing secret are envelope-encrypted in the vault (t43) and resolved BY HANDLE — there is
-- structurally no column on `billing_subscriptions` a card / token / provider key could ride in.
--
-- AT-LEAST-ONCE WEBHOOKS: provider events arrive at-least-once. `billing_events` is the idempotency
-- ledger — the provider's event id is the PRIMARY KEY, so a replayed "subscription cancelled" is a
-- no-op INSERT-OR-IGNORE and the plan state is never double-applied (the binary's apply path checks
-- this ledger inside the same transaction as the upsert).
--
-- APPEND-ONLY: migrations #1–#11 are FROZEN — the checksum guard forbids editing a shipped migration
-- in place; this ships as a NEW version (#12). The rusqlite read/write that fills these columns lives
-- in the binary-injected `SysBackend` (`crates/qfs/src/sys.rs`); this migration only declares the
-- shape. AUTHORIZATION NOTE: a `/sys/billing` write is high-privilege (super-admin); the
-- local-super-admin vs. project-admin split is deliberately NOT baked in yet (roadmap §3.4). The
-- payment PROVIDER itself is a flagged open product decision (no vendor baked in).
CREATE TABLE IF NOT EXISTS billing_subscriptions (
    team_id            TEXT PRIMARY KEY,
    tier               TEXT NOT NULL,
    status             TEXT NOT NULL,
    current_period_end TEXT,
    updated_at         TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);

CREATE TABLE IF NOT EXISTS billing_events (
    event_id   TEXT PRIMARY KEY,
    team_id    TEXT NOT NULL,
    applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);
