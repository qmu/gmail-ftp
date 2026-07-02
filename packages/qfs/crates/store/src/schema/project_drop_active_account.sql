-- Migration #11 (EPIC 20260702120000 / ADR 0008 §4 — ticket 20260702120050): DROP the
-- `active_account` selection table. Selection state is ABOLISHED: a cloud mount created by
-- `qfs connect <path> <kind> <account>` carries its own (host, driver, account) coordinate
-- (migration #9's `path_binding` columns), and the bind path resolves the account FROM THE
-- MOUNT — N accounts of one driver coexist as N paths, so there is nothing left to "select".
-- The shipped migration #2 body (which created the table) stays frozen (the checksum guard);
-- this append-only version removes the table forward, exactly like #10 superseded
-- `secret_meta`. Pre-release hard break: any persisted selection is deliberately discarded.
DROP TABLE IF EXISTS active_account;
