# qfs architecture (Rust workspace)

This document records the **crate-boundary rules** of the `qfs` Rust rebuild. It is
the durable companion to [`RFD-0001`](.workaholic/RFDs/0001-qfs-architecture.md) (the
single source of truth) and to the E0 trip artifacts under
`.workaholic/trips/qfs-foundation-e0/`. Every later ticket must add code **inside**
these boundaries without restructuring the workspace.

> Note: the legacy Go `gmail-ftp` program (`main.go`, `internal/`, `go.mod`) lives
> alongside this workspace and is untouched. Per RFD §0 it is *subsumed as a future
> driver*, not merged. The Rust workspace coexists with it.

## Crate map

| Crate (`crates/<dir>`) | package | Role (RFD §) |
|---|---|---|
| `qfs` (bin) | `qfs` | The single binary; thin `main.rs` calling `qfs_cmd::run` (§9). |
| `cmd` | `qfs-cmd` | argv parsing (clap-derive), dispatch into the engine; **no domain logic** (§7). |
| `core` | `qfs-core` | Shared engine glue: 3 registries, `Engine`/`Session`, re-exports, `CfsError` (§3/§6). |
| `lang` | `qfs-lang` | The frozen reserved-keyword closed core (§3); AST lands here in E1. |
| `plan` | `qfs-plan` | Effects-as-data: the typed `Plan` DAG of `EffectNode`s, `PlanApplier`/`commit`, and `PREVIEW` rendering (§3/§6/§10). Depends on `qfs-types` (leaf) for the row model (t09). |
| `driver` | `qfs-driver` | The `Driver` contract (mount + `id()` plan identity, archetype, typed `Schema`, capabilities + parse-time gate, `ProcSig`/pushdown/prelude/`@version`, the `applier()` seam) + owned DTOs (incl. the out-of-crate `Capabilities` builder); owns `CfsError` & `Path` (§5/§9). |
| `codec` | `qfs-codec` | The pure `bytes ↔ rows` `Codec` contract (§4). |
| `types` | `qfs-types` | The canonical type & schema model: `Value`/`Row`/`RowBatch`, `Schema`/`ColumnType`, schema algebra + typed predicates (§4/§5). **Leaf** crate (t05). |
| `server` | `qfs-server` | The server face: `serve` stub + `/server` mount seam (§8). |
| `parser` | `qfs-parser` | Parser front door skeleton; **filled by t02** (§2.2/§9). |
| `pushdown` | `qfs-pushdown` | The pushdown planner (t14, Domain): `LogicalPlan`/`PhysicalPlan`/`ScanNode`/`PushedQuery`, `partition_by_source` (source-split + federation), AST→IR lowering (predicates sourced from the AST, O-t07-3), and `explain()`. Pure; no I/O (§6). |
| `engine` | `qfs-engine` | The local combine engine (t14, Infrastructure): the `CombineEngine` seam + in-house `MiniEvaluator` running the cross-source residual (filter/project/hash-join/set-ops/aggregate/sort/limit/EXPAND). DuckDB rejected, ADR-0002; dependency-light + wasm-clean (§6/§9). |
| `txn` | `qfs-txn` | The transactional correctness envelope (t11): `EffectKey` idempotency, `@version`/ETag preconditions, `CommitStrategy`, saga/ACID executors, audit ledger, `RecoveryReport`. **Pure orchestration** over `qfs-plan`/`qfs-types` — carries **no** tokio of its own (§6). |
| `runtime` | `qfs-runtime` | The async effect interpreter (t10): `Interpreter::commit`, `ApplyDriver`/`DriverRegistry`, `EffectError` taxonomy, `CapabilitySet` gate, the sync→async `PlanApplierBridge`/`SharedApplier`. **tokio is CONFINED here** — the sole impure stage; nothing in the pure spine depends back onto it (§3/§5/§6). |
| `driver-local` | `qfs-driver-local` | The **first concrete driver** (t16): a blob/namespace driver over the host filesystem mounted at `/local` (`ls cp mv rm` + `upsert`/`remove`), least-privilege sandbox, streaming reads, atomic temp+rename writes, copy→verify(size+hash)→[delete]. A **LEAF runtime consumer** — it bridges its synchronous `PlanApplier` to the async `ApplyDriver`; tokio dead-ends here (§5/§6/§10). |
| `secrets` | `qfs-secrets` | The credential / secret store + multi-account resolution (t27): consumer-side, owned-DTO `Secrets` surface reusing the canonical `qfs_types::DriverId`. Depends on `qfs-types` only; `qfs-core` threads a `Secrets` handle into the driver-bind context (§10). |
| `sql-core` | `qfs-sql-core` | The **pure-leaf SQL compile/emit core** (t23 extraction from t17): the dialect-agnostic, **injection-safe** qfs-query → parameterized-SQL machinery (`Dialect` quoting/placeholders/upsert, the `SelectPlan`/`DmlOp` emitter that binds **every** value as a parameter, the pure `compile` query→plan compiler with a truthful residual) and the owned catalog DTOs (`Catalog`/`TableCatalog`/`ColumnDef`) + `SqlError`. The **single source of truth** for the sqlite emitter that **both** `qfs-driver-sql` (postgres/mysql/sqlite, t17) and `qfs-driver-cf` (Cloudflare D1, SQLite-over-HTTP, t23) reuse — so neither runtime-leaf driver depends on the other (the same single-source pattern as `qfs-http-core`). A pure leaf: its only workspace dep is `qfs-types`; carries **no** runtime/secrets/driver coupling. The runtime/secrets `From` adapters for `SqlError` live in the consuming driver crates (orphan-rule-safe explicit converters), not here (§5/§6). |
| `http-core` | `qfs-http-core` | The shared **pure-leaf** HTTP DTO + redaction crate (t19 refinement): the **single source of truth** for the vendor-free HTTP exchange DTOs (`HttpMethod` (the `#[non_exhaustive]` 4-variant set), `HttpRequest`, `HttpResponse`) and the header-redaction authority (`SENSITIVE_HEADERS` / `is_sensitive_header` + the redacting `Debug` impls). Carries **no** `reqwest`/`tokio`/`qfs-runtime` — its only workspace dep is `qfs-secrets` (the one `REDACTED` marker), so the closure is `qfs-http-core → qfs-secrets → qfs-types`. Both `qfs-driver-http` (real reqwest `HttpClient`) and `qfs-google-auth` (local `HttpExchange` seam) depend on it for ONE redaction set instead of each hand-copying a drift-prone duplicate (the prior hazard: a sensitive header added on one side and lagging on the other leaks a header *value* across the seam). The concrete transports stay in their own crates (§5/§10). |
| `google-auth` | `qfs-google-auth` | The **shared Google OAuth2 + multi-account auth base** (t19): the loopback authorization-code flow (`localhost`-not-`127.0.0.1` redirect — the load-bearing detail), access-token exchange/refresh, per-account refresh-token storage via `qfs-secrets` (keyed `(google, <encoded-email>)`), profile-email account identity, and the reusable `TokenSource` + authenticated `GoogleApiClient` (bearer inject, refresh-on-401) the Gmail (t20)/Drive (t21)/Analytics (t41) drivers build on. Scope-agnostic; tokens are `qfs_secrets::Secret`. Among workspace crates it depends on **`qfs-secrets` + `qfs-http-core` only** (both pure leaves reaching no further than `qfs-types`) — network rides a thin, synchronous, runtime-free `HttpExchange` seam (over the shared `qfs-http-core` DTOs) kept **local** so this crate does NOT depend on `qfs-driver-http` (a runtime leaf); the consuming drivers adapt their `Arc<dyn qfs_driver_http::HttpClient>` to `HttpExchange`. The HTTP DTOs + header redaction now come from the shared `qfs-http-core` leaf (single source of truth), not a local copy. Constructs no `Plan`/effect; `authorize` is native-only (feature-gated off `wasm32`) (§5/§6/§10). |
| `driver-gmail` | `qfs-driver-gmail` | The **first real `Driver`** (t20, RFD §5): subsumes the legacy Go `gmail-ftp` as one mount at **`/mail`** under the uniform VFS + pipe-SQL DSL, mapping the mailbox onto the **Append/log archetype** (labels = directories, messages = files, attachments = nested). Implements the t13 introspective contract (describe/capabilities/procedures/prelude/pushdown/applier) over the t19 `GoogleApiClient`. **Path-keyed least-privilege capabilities** (`/mail/drafts` = Insert\|Upsert\|Select\|Remove; `/mail/<label>` = Select\|Update\|Remove; a message = Select\|Remove; a thread = Remove only). SELECT pushes WHERE/LIMIT to the Gmail search `q=` with a **truthful local residual** — only `label`/`is_unread` map exactly; `from`/`to`/`subject` `=`/`~`/`LIKE` and the `date` bounds are looser Gmail operators pushed as a pre-filter and **re-checked locally** (over-fetch then filter, never wrong rows — RFD §6). **REMOVE = TRASH** (the `GmailClient` trait has no permanent-delete method at all); `CALL mail.send` (and the `SEND` prelude alias) is the `irreversible` transition (create-draft-then-send de-dupe). **Least-privilege scopes**: `gmail.modify` + `gmail.compose` only (no `mail.google.com`, no delete). **No vendor leak** — Gmail JSON decodes to owned DTOs at the mockable `GmailClient` boundary; `reqwest` stays in `qfs-driver-http`. Tokens are `qfs_secrets::Secret` behind the auth base — never logged, never in a `GmailError`. A **LEAF runtime consumer** — bridges its synchronous `PlanApplier` to the async `ApplyDriver` via `qfs-runtime`'s `PlanApplierBridge`; tokio dead-ends here. Parked for t20: attachment-bytes on-demand fetch, `historyId`/`@version` sync (E7), and the live smoke test (§5/§6/§10). |
| `driver-cf` | `qfs-driver-cf` | The **Cloudflare driver** (t23, RFD §5): one driver mounted at **`/cf`** fanning out to three Cloudflare primitives, each on its correct archetype. **D1** (`/cf/d1/<db>/<table>`, RelationalTable) is SQLite-over-HTTP — it **reuses the `qfs-sql-core` `Dialect::Sqlite` emitter + compiler** (the same injection-safe parameterized SQL t17 renders) and ships the rendered `(sql, params)` to the Cloudflare D1 REST API with `params` as a **structured bound array** (never interpolated); the D1 `/batch` endpoint maps one `commit_transaction` to **one atomic transaction** (D1 has no interactive BEGIN/COMMIT). **KV** (`/cf/kv/<ns>/<key>`, BlobNamespace): `ls/cp/mv/rm` + a degenerate `(key,value)` table for `SELECT`/`UPSERT`, TTL + metadata. **Queues** (`/cf/queue/<name>`, AppendLog): `INSERT` appends with an idempotency key (at-least-once-safe), `SELECT … LIMIT n` tails. Per-node capabilities gate verbs at parse time (e.g. `UPDATE`/`JOIN` over a queue/KV rejected structurally). The Cloudflare API token is a `qfs_secrets::Secret` written into a redacted `Authorization: Bearer` header — never logged, never in a `CfError`. **No vendor leak** — Cloudflare JSON + `worker::*` bindings decode to owned DTOs at the mockable `CfBackend` seam; the HTTP path rides a **local `HttpExchange` seam** over `qfs-http-core` DTOs (the `qfs-google-auth` precedent), so this crate does NOT depend on `qfs-driver-http` and stays an **independent runtime leaf** (it reuses the pure `qfs-sql-core`, not the `qfs-driver-sql` runtime leaf). Bridges its synchronous `PlanApplier` to the async `ApplyDriver`; tokio dead-ends here. Parked: the wasm `WorkersBindingBackend` (same seam) + live Cloudflare integration (§5/§6/§10). |

## Dependency spine (acyclic, no back-edges)

```
qfs (bin) → qfs-cmd → qfs-core → { qfs-lang, qfs-plan, qfs-driver, qfs-codec, qfs-types }
                         ▲
                   qfs-server ── depends on qfs-core
qfs-codec  → qfs-driver        (shared CfsError, decision D1 — acyclic)
qfs-codec  → qfs-types         (canonical Value/Row/RowBatch row model, t05 — acyclic)
qfs-driver → qfs-plan          (Driver::applier returns the PlanApplier seam; Plan nodes)
qfs-driver → qfs-types         (Driver::describe returns the canonical typed Schema, t13)
qfs-plan   → qfs-types         (effect nodes carry the canonical RowBatch/DriverId, t09 — acyclic)
qfs-txn    → { qfs-plan, qfs-types }  (t11 transactional envelope: PURE orchestration — EffectKey
                                idempotency, @version/ETag preconditions, CommitStrategy, saga/ACID
                                executors, audit ledger, RecoveryReport; NO tokio of its own)
qfs-runtime → { qfs-plan, qfs-types, qfs-txn }  (t10/t11 — tokio CONFINED here; the runtime bridges
                                its async ApplyDriver to qfs-txn's synchronous LegApplier seam)
qfs-parser → qfs-lang          (consumes the frozen keyword consts / AST)
qfs-pushdown → { qfs-driver, qfs-types, qfs-plan, qfs-parser }  (t14 pushdown planner: PURE.
                                PushdownProfile accessors from qfs-driver; the typed Predicate
                                + Schema::join from qfs-types; AST lowering from qfs-parser. No
                                I/O/async/vendor — acyclic, below qfs-core.)
qfs-engine → { qfs-pushdown, qfs-types }  (t14 local combine engine: the MiniEvaluator over the
                                PhysicalPlan. Dependency closure = serde family only; wasm-clean.)
qfs-core   → qfs-pushdown       (t14 integration seam: qfs_core::plan wires query AST → LogicalPlan
                                → PhysicalPlan via the live MountRegistry; surfaces ScanNodes for T10.)
qfs-secrets → qfs-types        (t27 credential/secret store + multi-account resolution: consumer-side,
                                owned-DTO, reuses the canonical DriverId. LEAF over qfs-types — acyclic.)
qfs-core   → qfs-secrets       (t27 bind-context credential surface: the Engine threads a Secrets
                                handle into the driver-bind context; qfs-core re-exports it.)
qfs-http-core → qfs-secrets    (t19 refinement: the shared PURE-LEAF HTTP DTO + redaction crate.
                                Owns HttpMethod/HttpRequest/HttpResponse + SENSITIVE_HEADERS/
                                is_sensitive_header + the redacting Debug — the SINGLE source of
                                truth for both HTTP seams. Among workspace crates it depends on
                                qfs-secrets ONLY (the REDACTED marker; reaches qfs-types only), and
                                carries NO reqwest/tokio/qfs-runtime — so depending on it does not
                                pull either HTTP crate toward the runtime. Acyclic leaf.)
qfs-driver-http → qfs-http-core (t19 refinement: the REST driver's request/response DTOs +
                                redaction now come from the shared leaf, not a local copy; its
                                HttpClient trait + reqwest impl stay local.)
qfs-google-auth → { qfs-secrets, qfs-http-core }  (t19 shared Google OAuth + multi-account auth
                                base. Among workspace crates it depends on qfs-secrets + qfs-http-core
                                ONLY (both reach no further than qfs-types), so it stays OFF
                                qfs-runtime: network rides a local, runtime-free HttpExchange seam
                                (over the shared qfs-http-core DTOs) rather than qfs-driver-http
                                (which depends on qfs-runtime and must remain a leaf). The HTTP DTOs
                                + redaction are single-sourced in qfs-http-core, not hand-copied.
                                Acyclic; not on the spine — consumed only by the future Google
                                driver leaves (t20/t21/t41).)
qfs-driver-local → { qfs-driver, qfs-plan, qfs-types, qfs-codec, qfs-runtime }  (t16 FIRST concrete
                                driver; LEAF runtime consumer — bridges the synchronous PlanApplier →
                                async ApplyDriver and registers it in the DriverRegistry. Nothing
                                depends back onto it, so tokio dead-ends here and never re-enters the
                                spine — the precedent the next 11 drivers follow.)
qfs-driver-gmail → { qfs-driver, qfs-plan, qfs-types, qfs-codec, qfs-runtime, qfs-google-auth,
                                qfs-http-core }  (t20 FIRST real Driver, mount /mail; LEAF runtime
                                consumer following the qfs-driver-local precedent — bridges its
                                synchronous PlanApplier → async ApplyDriver via qfs-runtime, so tokio
                                dead-ends here and never re-enters the spine. Rides the t19 auth base
                                (GoogleApiClient over the local HttpExchange); NO reqwest — that stays
                                in qfs-driver-http. Nothing depends back onto it.)
qfs-sql-core → qfs-types       (t23 extraction: the PURE-LEAF SQL compile/emit core — the
                                Dialect::Sqlite emitter + pure compiler + catalog DTOs + SqlError,
                                single-sourced so BOTH qfs-driver-sql and qfs-driver-cf reuse one
                                emitter. Among workspace crates it depends on qfs-types ONLY; carries
                                NO runtime/secrets/driver coupling. Acyclic leaf.)
qfs-driver-sql → { qfs-driver, qfs-plan, qfs-types, qfs-runtime, qfs-secrets, qfs-sql-core }  (t17
                                SQL driver; LEAF runtime consumer. Now reuses the pure qfs-sql-core
                                emitter rather than owning it; the runtime/secrets From adapters for
                                SqlError stay here.)
qfs-driver-cf → { qfs-driver, qfs-plan, qfs-types, qfs-runtime, qfs-sql-core, qfs-http-core,
                                qfs-secrets }  (t23 Cloudflare driver, mount /cf; LEAF runtime
                                consumer. Reuses the PURE qfs-sql-core (NOT qfs-driver-sql, a runtime
                                leaf) for the D1 sqlite emitter, and rides a LOCAL HttpExchange seam
                                over qfs-http-core (NOT qfs-driver-http) — so it stays an INDEPENDENT
                                runtime leaf and neither runtime leaf depends on the other. Nothing
                                depends back onto it.)
qfs-types  → (serde only)      (LEAF: no workspace deps; the vendor-free type model, t05)
```

Arrows point toward more-foundational crates. There are **no cycles** and **no
back-edges**. Mechanically enforced by `crates/cmd/tests/dep_direction.rs` (a
`cargo metadata` test).

### tokio confinement — the generic runtime-leaf rule (t10/t16)

`qfs-runtime` is the sole impure stage; tokio/futures live there and MUST NOT enter the
pure spine's closure (the invariant that keeps `qfs-plan`'s purity dep-closure test green).
`dep_direction.rs::runtime_is_confined_to_plan_and_types` enforces this in three parts:

1. **(a) source pinned** — `qfs-runtime`'s own workspace deps are exactly `{qfs-plan,
   qfs-types, qfs-txn}` (and `qfs-txn` is itself pure, carrying no tokio), so tokio's source
   is one crate.
2. **(b) generic leaf confinement** — **every** crate that depends on `qfs-runtime` must be a
   **leaf**: no workspace crate may depend back onto it. This encodes *why* a runtime consumer
   is safe (tokio dead-ends in a leaf and cannot transit back into the spine) rather than
   *which* crates were waved through. A non-leaf gaining the edge (e.g. `qfs-core → qfs-runtime`)
   fails automatically — **no per-driver test edit is needed** as the next 11 driver crates land.
3. **(b') identity allowlist** — a small named allowlist (`qfs-driver-local`, `qfs`) pins the
   *intent*: an unintended new runtime consumer is caught even if it happens to be a leaf today.
   A new driver-impl leaf appends its name here (a one-line, reviewable signal); the generic
   leaf check guarantees the append was safe.

`qfs-driver-local` is the first such leaf consumer: it bridges its synchronous `PlanApplier`
to the async `ApplyDriver` and registers it in the `DriverRegistry`.

### Decision D1 — where `CfsError` / `Path` live

design-v1 nominally placed `CfsError` and `Path` in `qfs-core`, but the `Driver` and
`Codec` trait signatures return `Result<_, CfsError>` and take `&Path`, while the
spine requires `qfs-core → qfs-driver`. Putting them in `qfs-core` would force a
cycle. They therefore live in **`qfs-driver`** (the lowest crate the signatures
need); `qfs-codec` depends on `qfs-driver` for the shared error, and `qfs-core`
**re-exports** both so the workspace still sees one `qfs_core::CfsError` /
`qfs_core::Path`. This preserves "one error enum, shared workspace-wide" while keeping
the spine strictly acyclic.

### Decision D2 — the `qfs-types` leaf (t05)

The canonical type & schema model (`Value`/`Row`/`RowBatch`, `Schema`/`ColumnType`,
`TypeError`, schema algebra, typed predicates) lives in a dedicated **leaf** crate
`qfs-types` (pre-reserved by the Architect). E0 shipped placeholder `Value`/`Row`/
`RowBatch` in `qfs-codec`; t05 promotes them to the canonical typed model and
`qfs-codec` now **re-exports** them from `qfs-types`, so the `bytes ↔ rows` boundary
and the evaluator speak one row model. The crate depends on **no other workspace
crate** (only `serde`/`serde_json`, the latter solely as the carrier for the `Json`
value of irregular columns), keeping it the lowest node in the spine and the type
model vendor-free (RFD §9, G6). To preserve that leaf status, `DriverId` is defined
*inside* `qfs-types` (an owned newtype, never a vendor handle) rather than imported
from `qfs-driver`, and the `SchemaSource` trait takes a logical segment list
(`&[Name]`) instead of the driver `Path`; E4 adapts the driver `Path` at the boundary.
`qfs-core` depends on `qfs-types` and re-exports it so the workspace sees one
`qfs_core::Schema` / `Value` / `TypeError`. Mechanically enforced by
`dep_direction.rs::types_is_a_leaf_and_codec_depends_on_it`.

### Wired edge — `qfs-core → qfs-parser` (acceptance criterion C5, E1)

The edge is now **wired** (E1): name resolution (`qfs_core::resolve`, t06) and the pure
evaluator (`qfs_core::eval`, t07) both consume the parsed `qfs_parser::Statement` AST —
the resolver binds `CALL`/alias names and gates effect verbs, and the evaluator folds a
statement into a `qfs_plan::Plan` (effect-plan) + logical `PlanSource` relation. The edge
is one-directional so `qfs-parser` can never depend on `qfs-core` (cycle prevention).
`dep_direction.rs::core_depends_on_parser_one_directionally` asserts the edge is present
and the back-edge absent.

## Boundary rules (must hold for every later ticket)

1. **Closed core / one keyword home (G1).** The reserved-keyword set lives only in
   `qfs-lang::KEYWORDS`. A new backend adds *zero* keywords — it registers a path,
   procedure, or codec instead. The freeze test (`qfs-lang`) locks the set.
2. **Open registries generic over trait objects (G2).** Extension = `register(...)`
   into one of the three `qfs-core` registries. Registries hold `Arc<dyn Driver>` /
   `Arc<dyn Codec>` / owned `ProcedureDecl`, never concrete types. `MountRegistry`
   additionally routes a full path to a driver by **longest mount-prefix**
   (`MountRegistry::resolve_path`), so overlapping mounts (`/g`, `/git`) resolve
   deterministically and the matched mount is stripped to a driver-local sub-path.
3. **Purity invariant at the type level (G3).** `Driver` / `Codec` methods return
   data (or `Plan` nodes); none take `&mut self`, return a future, or perform I/O. The
   only impure op (`COMMIT : Plan -> World`) is reserved for E2 and absent from the
   traits. Proven by the no-I/O dummy-impl tests in `qfs-driver` / `qfs-codec`.
4. **Server is a driver (G4).** `qfs-server` reserves `/server` as a mount and a
   `TODO(E7)` anchor; the server must be registered as a `Driver`, never a bespoke
   subsystem.
5. **cmd is logic-free (G5 / C4).** `qfs-cmd` depends on `qfs-core` + `qfs-server`
   only. Enforced by `dep_direction.rs`.
6. **Parser boundary reversibility (G6).** The chosen parser library's types never
   appear in `qfs-parser`'s public API — wrapped behind an owned `ParseError` and the
   `parse_statement` signature (t02).
7. **No vendor leak / owned DTOs (B3).** No SDK/vendor type crosses the `qfs-driver`
   boundary. DTOs are owned and `#[non_exhaustive]` with `new` constructors.
8. **No credentials (B8).** No secret/credential field anywhere; none in tests or CI.

## wasm-friendliness (B7)

The core crates (`qfs-core`, `qfs-lang`, `qfs-plan`, `qfs-driver`, `qfs-codec`,
`qfs-parser`) avoid threads, `std::fs`, and sockets so the future `wasm32` target
(RFD §1/§9) stays cheap. All real I/O lives behind (future) driver impls. `unsafe`
code is `forbid`-den workspace-wide.

## Cross-compile status

- **native `aarch64-unknown-linux-gnu`**: built & tested locally (this host is
  Graviton/aarch64). This is the local proof.
- **`x86_64-unknown-linux-gnu`**: lib crates cross-compile; the full binary link is
  **CI-only** (no x86_64 cross-linker on the local aarch64 host).
- **`wasm32-unknown-unknown`**: **deferred** per t01 — not built locally or in CI yet
  (a parked t02 / future-E0-sibling concern). Code is kept wasm-friendly per B7.

## Lints

Workspace lints (`Cargo.toml` `[workspace.lints]`): `unsafe_code = forbid`; clippy
`all = deny` plus `unwrap_used` / `expect_used` / `panic = deny` on non-test lib code.
Test modules opt out via `#![cfg_attr(test, allow(...))]` (and integration tests via a
file-level `#![allow(...)]`). Gates: `cargo fmt --all --check`, `cargo clippy
--workspace --all-targets --all-features -- -D warnings`, `cargo build --workspace`,
`cargo test --workspace`.
