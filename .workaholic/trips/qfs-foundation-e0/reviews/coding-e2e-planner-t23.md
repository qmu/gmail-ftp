# Coding Phase — E2E / External Testing — t23 (Cloudflare D1 / KV / Queues driver)

- Author: Planner (Progressive)
- Role: E2E / external interface testing only (no code review)
- Target: t23 — `qfs-driver-cf` (`qfs_driver_cf`), mount `/cf`
- Method: a throwaway external consumer crate (`/tmp/cf-e2e`, own `[workspace]`, path-deps on
  `driver-cf`, `runtime`, `driver`, `plan`, `types`, `secrets`, `sql-core`, `http-core`). It supplies
  the Planner's OWN mock `CfBackend` (records every request, returns canned responses) — NO live
  Cloudflare, NO network, NO production code. Crate removed after the run.
- Verdict: **E2E approved**

## How validated

The harness only touches the public surface (`qfs_driver_cf::{CfDriver, CfApplier, CfBackend,
CfRegistry, D1Database, KvEntry, QueueMsg, MsgId, CfError, HttpApiBackend, MockExchange,
cf_apply_driver, ...}`). A Planner-owned `ExternalMockBackend` implements `CfBackend` and records the
verbatim arguments the driver issued, so every claim below is observed at the genuine external
boundary. All 7 required items PASS (plus a multi-statement atomicity sub-check).

## PASS/FAIL per item

### 1. D1 SELECT + injection safety (BLOCKING) — PASS

A `SELECT id, name FROM /cf/d1/prod/users WHERE name = '; DROP TABLE x; --'` was issued. The
recorded D1 request:

```
db     = prod
sql    = SELECT "id", "name" FROM "users" WHERE "name" = ?
params = [Text("'; DROP TABLE x; --")]
```

- SQL carries a `?` placeholder only; identifiers are quoted.
- The injection literal is **NOT** in the SQL text (no `DROP TABLE`, no `'; ... --`).
- The value rides in a SEPARATE structured `params` array as `Param::Text(...)`, inert as data.

This is the blocking guarantee and it holds: no value-in-SQL-string interpolation.

### 2. D1 effects + batch atomicity — PASS

INSERT / UPDATE / DELETE each recorded as a D1 **batch** request (`d1.batch`), one statement per
atomic batch, all parameterized:

```
batch db=prod : ["INSERT INTO \"users\" (\"id\", \"name\") VALUES (?, ?)"]
batch db=prod : ["UPDATE \"users\" SET \"name\" = ? WHERE \"id\" = ?"]
batch db=prod : ["DELETE FROM \"users\" WHERE \"id\" = ?"]
```

The INSERT bound the injection value into `params` (never the SQL). A multi-statement write driven
through the backend seam is recorded as **one** `D1Batch` carrying 2 statements (sub-check `2b` PASS)
— one atomic transaction, matching the D1 `/batch` = one transaction contract.

### 3. KV PUT (TTL/metadata) / GET / DELETE / list — PASS

Recorded calls and decoded results:

```
KvPut   { ns: "cache", entry: { key: "k3", value: "v3", metadata: Some("{\"x\":1}"), expiration_ttl: Some(60) } }
KvGet   { ns: "cache", key: "k1" }   -> value "v1", metadata "{\"tag\":\"a\"}", ttl Some(3600)
KvDelete{ ns: "cache", key: "k3" }
KvList  { ns: "cache", prefix: Some("k"), limit: Some(10) }  -> ["k1", "k2"]
```

PUT carries TTL + metadata; GET round-trips value+metadata+TTL; DELETE targets the right key; list
returns the decoded keys with prefix/limit forwarded.

### 4. Queues send (idempotency key) + pull/consume — PASS

```
QueueSend { queue: "events", body: "payload", idempotency_key: "evt-42" }
QueuePull { queue: "events", max: 1 }   -> [QueueMsg { id: "m1", ... }]
```

The send carries the idempotency key (at-least-once de-dupe); pull/consume tails capped at `max`.

### 5. Capability — structural rejection — PASS

```
KV    UPDATE -> Err(UnsupportedVerb { path: "/cf/kv/cache",    verb: "UPDATE", supported: ["SELECT","UPSERT","REMOVE","LS","CP","MV","RM"] })
QUEUE UPDATE -> Err(UnsupportedVerb { path: "/cf/queue/events", verb: "UPDATE", supported: ["SELECT","INSERT"] })
```

A WRITE (UPDATE) against a KV namespace node and an UPDATE against a queue are both rejected
structurally with code `unsupported_verb` (no panic). Targeted, not blanket: `Upsert` on KV and
`Insert` on the queue still pass the gate.

### 6. Token safety — PASS

A canary token `PLANTED-CF-TOKEN-cafef00d-...` was planted into an `HttpApiBackend` and into every
`CfError` variant's Debug/Display. The token (and the `cafef00d` fragment) is absent from all error
surfaces. The request Debug redacts the bearer:

```
HttpRequest { ..., headers: [("Authorization", "***redacted***")], body_len: 0 }
```

No panics anywhere in the run.

### 7. End-to-end COMMIT through interpreter + bridge (D1 write + KV put) — PASS

A plan (`INSERT /cf/d1/prod/users` + `UPSERT /cf/kv/cache`) committed through
`Interpreter::commit` over `cf_apply_driver` bridge:

```
outcome.is_complete() = true ; ledger entries = 2
  NodeId(0) Insert -> Applied { affected: 1, attempts: 1 }
  NodeId(1) Upsert -> Applied { affected: 1, attempts: 1 }
recorded backend calls:
  D1Batch { db: "prod", statements: [("INSERT INTO \"users\" (\"id\", \"name\") VALUES (?, ?)", [Int(9), Text("dave")])] }
  KvPut   { ns: "cache", entry: { key: "sess", value: "xyz", ... } }
```

Ledger records both legs applied; recorded calls confirm the D1 batch and KV put actually reached
the backend through the bridge.

## Concern + proposal (Critical Review Policy)

- Concern (business outcome): the injection-safety guarantee currently rests on the `param_to_json`
  rendering and the t17 emitter never inlining a value. For an AI-driven product where untrusted
  natural-language values flow straight into D1 predicates, this single seam is the whole safety
  story — a future emitter change could regress it silently with no failing test outside the crate.
- Proposal: keep at least one params-as-array assertion as a **public, externally-runnable**
  conformance check (golden) so the injection invariant is guarded from the consumer side, not only
  by internal unit tests — preserving stakeholder-traceable proof that values are never interpolated.

## Overall verdict

**E2E approved.** No interpolated value (no injection), no token leak, no panics. All seven required
items pass against a mocked Cloudflare backend with no live network.
