-- Project DB — migration #9 (EPIC 20260702120000 / ADR 0008 — the multi-host account model):
-- the MOUNT COORDINATE columns on the defined-path binding registry.
--
-- APPEND-ONLY: migrations #1..#8 are FROZEN (the checksum guard forbids editing a shipped
-- migration); these columns ship as a NEW version ALTERing `path_binding` forward.
--
-- ADR 0008 §4: selection state is abolished — the MOUNT carries the full (host, driver, account)
-- coordinate, so a statement's target is readable from the statement alone and two accounts of one
-- driver coexist as two paths. This migration only RESERVES the dimension (groundwork): nothing
-- reads these columns for bind resolution until the mount-bound-accounts ticket (20260702120050)
-- rewires `networked_credential` / `resolve_account_email` off the `active_account` selection.
--
-- SELECTORS + METADATA ONLY — never a secret. `account` is an account LABEL (e.g. an email for a
-- Google account: the `google:<email>:refresh_token` key's email part), never a token. `host` names
-- which qfs host owns the mount; `'local'` is the implicit embedded host (ADR 0008 §1) — remote
-- host records land with the hosts skeleton (20260702120060).
ALTER TABLE path_binding ADD COLUMN host TEXT NOT NULL DEFAULT 'local';
ALTER TABLE path_binding ADD COLUMN account TEXT;
