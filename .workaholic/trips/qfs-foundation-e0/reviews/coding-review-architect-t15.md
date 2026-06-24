# Coding Review (Architect) — t15 Codec registry: DECODE/ENCODE + struct/array model

- Reviewer: Architect (Neutral / structural bridge)
- Target: t15 — Codec registry + struct/array model (commit `33f8821`, branch `work-20260622-230954`)
- Scope: analytical review only (no cargo/test execution)
- Files read: `crates/codec/src/{lib,convert,nested}.rs`, `crates/codec/src/codecs/{mod,json,jsonl,yaml,toml,csv,markdown}.rs`,
  `crates/core/src/registry.rs`, `crates/driver/src/error.rs`, `crates/codec/Cargo.toml`,
  `crates/types/src/{value,schema,unify}.rs`, `crates/codec/tests/codecs.rs`, `ARCHITECTURE.md`, ticket t15.

## Decision: Approve with observations

The codec layer is structurally sound and is the right realization of RFD §4: six pure `bytes ↔ rows`
codecs funnel through one `serde_json::Value`↔`qfs-types` bridge, the vendor parsers are genuinely
confined, the codec registry is the third open registry with structured `UnknownCodec`, and
`with_builtins` is wired correctly. I am approving with observations rather than requesting revision
because nothing *built here* is a structural defect — the two issues below are (1) a **latent
correctness trap inherited from the t05 `Value::Struct` model**, not introduced by t15, and (2) a
**deferred `infer_schema`/`DESCRIBE` seam** the ticket sketched but the team folded into `decode`.
Both should be recorded as explicit carry-overs so the E4 drivers (t16 fs, t18 http, t26 git) are not
built against an ambiguous nested-data contract. Per Critical Review Policy I raise both with concrete
proposals.

## What is right (structural confirmation)

1. **G6 / boundary — no vendor leak (confirmed mechanically).** `mod convert;` and `mod nested;` are
   **private** (lib.rs:19-21); `convert` is *not* re-exported. The five `pub fn`s in `convert.rs` that
   name `serde_json::Value` are crate-internal (visible to sibling modules only). `access_json`
   (nested.rs:116, takes/returns `serde_json::Value`) is a private `fn`. The crate's public surface —
   the `Codec` trait, `access`/`access_row`/`expand`, the six codec structs, `builtin_codecs`, and the
   `qfs_types`/`CfsError` re-exports — carries **only owned `qfs-types` and `CfsError`** types. No
   `serde_json::Value` / `serde_yaml::Value` / `toml::Value` / `csv::*` appears in any pub signature
   crossing the crate boundary. RFD §9 / boundary B3 holds.

2. **Purity invariant (G3 / B7).** Every `decode`/`encode` takes `&self` and owned byte/row data and
   returns owned data or `CfsError`; no `&mut self`, no future, no `std::fs`/socket. `dummy_codec_is_pure`
   plus the `serde_*`/`toml`/`csv` parsers being in-memory keep the wasm-friendliness invariant intact.

3. **Registry fit (RFD §3).** `CodecRegistry` (registry.rs:154) is the third registry, identical in
   shape to `MountRegistry`/`ProcRegistry`: `BTreeMap<String, Arc<dyn Codec>>`, `register`/`resolve` by
   `fmt()` name, deterministic iteration, structured `UnknownCodec`/`DuplicateRegistration` (no panic).
   `with_builtins` loads the canonical `qfs_codec::builtin_codecs()` set (single source of truth in the
   codec crate, not duplicated in core) and swallows the *structurally unreachable* duplicate error
   instead of `unwrap`/`expect` — panic-free lib code, lint-clean. `codec_registry_with_builtins_resolves_all_six`
   asserts the six names resolve and `parquet` → `UnknownCodec`. G2 holds.

4. **Struct/array model + schema inference.** `rows_to_batch` folds `Schema::unify` over each row's
   structural schema, then `align_row` re-positions every row to the union (missing column → `Null`),
   so a heterogeneous JSON array yields a rectangular, conformant batch (`json_heterogeneous_rows_unify_...`
   asserts this and `is_conformant()`). `unify`/`widen` (t05) is total — `Unknown` is bottom, `Int∨Float⇒Float`,
   structs unify recursively, irreconcilable ⇒ `Json` — so inference never errors and degrades sanely,
   exactly as RFD §4 requires. Irregular JSON (huge numbers) degrades to `Value::Json` rather than failing.
   Encode determinism (RFD §6) is real: `row_to_json` emits keys in schema-column order, so `UPSERT`/diffs
   are stable. CSV widening (`widen_cell`: any scalar mismatch ⇒ `Text`, the lossless carrier) is correct.

5. **EXPAND semantics.** Value-level `expand` mirrors the type-level `Schema::expand`: absent/non-expandable
   field ⇒ passthrough clone; array ⇒ one row per element (empty array ⇒ zero rows, filtered);
   array-of-struct flattens element fields; struct ⇒ de-nest one level. The schema is computed once and
   the row splice matches it. Four tests assert the documented rules. "One EXPAND for mail attachments and
   JSON arrays" (RFD §4) is realized.

6. **Markdown+frontmatter ↔ row.** Fence-at-line-start splitting is correct (rejects `---x`, handles
   unterminated frontmatter leniently as body), frontmatter keys → columns + `body: Text`, encode reverses
   it deterministically. This is what makes `.workaholic/**/*.md` a relation, per the ticket's headline use case.

## Observation 1 (carry-over; the one that matters) — nested-struct field names are lost on the decode→access path, and it is untested

This is the report's admitted "nested-struct field-name loss," and my read is that it is **more than a
cosmetic round-trip caveat — it silently breaks `a.b.c` access over decoded data**, which t16/t18/t26 will
rely on. The mechanism:

- `Value::Struct` (t05) holds a bare `Row` with **no names** (`crates/types/src/value.rs`). The only place
  field names live for a nested struct is `Value::type_of()` → `Row::schema_of()`, which **fabricates
  positional names `"0","1",…`** (value.rs:120) because the row carries none.
- In `convert.rs::json_to_value` (lines 31-34), an object child is converted via `json_to_struct(node)`
  returning `(row, _schema)` — and the `_schema` carrying the **real keys is discarded**; only
  `Value::Struct(row)` survives. So the names are computed and then thrown away.
- Therefore decoding `{"user":{"name":"x"}}` yields a top-level column `user : Struct({"0": Text})` — the
  inner key `"name"` is already gone *at the type level*. `access_row(row, schema, ["user","name"])`
  (nested.rs:130) then returns `None`, because the nested struct's schema has a column `"0"`, not `"name"`.

The danger is that the **test suite hides this**: `path_access_navigates_nested_structs_without_flattening`
(tests/codecs.rs:367) builds its schema *by hand* with real names (`meta`/`inner`/`k`) and never goes through
a codec, so it proves `access` works *given* a named schema — but no test runs `decode → access` end to end.
`json_nested_object_becomes_struct_and_array_becomes_array` asserts only `matches!(… Struct(_))`, never the
inner column names. And `json_roundtrip_is_value_stable` uses a **flat** fixture, so it never exercises
nested-name fidelity through encode either. The limitation is real but **partially un-evidenced** by the
tests that appear to cover it.

Why this is not a "request revision": the root cause is the **t05 `Value::Struct(Row)` shape** (a names-free
row), not t15 code; fixing it properly is a model change (carry a schema inside `Value::Struct`, or thread the
batch schema into `access`), which is out of t15's scope and should be a deliberate decision, not a rushed
patch. The decode/encode/expand behavior t15 *did* build is correct for its model.

Proposals (pick one for the carry-over ticket; my preference is the first):
- **(a) Carry the field schema in the struct value.** Change `Value::Struct(Row)` → a shape that also holds
  the field schema (or `Value::Struct(BTreeMap<String,Value>)` as the ticket's own §"Key components" sketch
  named). Then `json_to_value` keeps the `_schema` it currently discards, `type_of` reports real names, and
  decode→`a.b.c`→encode all round-trip names. Highest fidelity; a t05 follow-up.
- **(b) Thread the column type into access.** Have the evaluator pass the *static* nested `Struct(Schema)`
  type (which `json_to_struct` *does* compute for the top-level column) down through `access`, instead of
  reconstructing names from the value. `access` already takes a `schema`; the gap is only that the value-side
  names disagree with it. Lower-cost, but leaves `Value` self-describing-by-position.
- **(c) At minimum, before either lands:** add a `decode → access_row(["a","b"])` test and a nested
  round-trip test so the limitation is *asserted* rather than implied, and tighten the doc on `access` to say
  "names come from the static schema, not the runtime struct value." This makes the trap visible to t16/t18/t26.

**Recommended carry-over:** record an explicit E4/t05 carry-over — *"nested-struct field names are not
recoverable from a decoded `Value::Struct`; `a.b.c` into a decoded nested object resolves positionally and
will not match real key names. Decide (a)/(b) before a driver ships `SELECT a.b.c` over decoded blobs."*
This is the analogue of the t05 two-schema-reconciliation carry-over and should sit beside it.

## Observation 2 (carry-over) — `infer_schema` / DESCRIBE-without-materialization was dropped; the trait deviates from the ticket sketch

The Constructor kept the established two-method `Codec` trait (`fmt`/`decode`/`encode`) and **folded schema
inference into `decode`** (every codec returns a `RowBatch` whose `.schema` is the inferred schema). The
ticket's §"Key components" sketched a richer trait — `decode(...)->RowSet` + `encode` + a separate
`infer_schema(bytes,opts)->Schema` "best-effort schema **without materializing all rows** (powers DESCRIBE)",
plus `DecodeOpts`/`EncodeOpts`. Implementation step 8 and the final acceptance bullet also call out
`infer_schema` (sample-based for jsonl/large json) explicitly.

My structural read: folding inference into `decode` is the **right minimal call for t15** and for an E0/E4
where blobs are bounded — it keeps one inference path (in `rows_to_batch`), avoids a second code path that
could disagree with `decode`, and is honest (no false promise of a cheap schema). The `DecodeOpts`/`EncodeOpts`
omission is also fine for now (CSV's `with_delimiter` covers the one real option). So this is **not** a defect.

But it leaves a real capability gap that a driver *will* hit: `DESCRIBE <path>` over a large blob (a 1 GB
JSONL, an S3 object) wants the schema **without decoding every byte into rows**. With inference welded to
`decode`, `DESCRIBE` forces a full materialization. The driver tickets (t16 fs / t18 http / t26 git) are the
first that surface `DESCRIBE`, so the decision should be visible to them, not silently absent.

Proposal: record an explicit carry-over to the driver/`DESCRIBE` ticket — *"add `Codec::infer_schema(bytes)
-> Schema` (sample-N for jsonl/json) when `DESCRIBE`-without-full-decode is needed; until then `DESCRIBE`
decodes."* Because `Codec` is a plain trait (not `#[non_exhaustive]` sealed), adding a *defaulted*
`infer_schema` later (default = `decode(bytes).map(|b| b.schema)`) is a non-breaking change, so deferring is
low-risk. Worth one sentence in the t15 report / ticket so the deviation from the sketch is a recorded choice,
not an oversight.

## Minor notes (non-blocking, no carry-over needed)

- **TOML/CSV multi-row encode asymmetry is correct but worth a doc line.** `TomlCodec::encode` emits only
  `rows.first()` (a TOML doc is one table) and `MarkdownFrontmatterCodec::encode` likewise. Decoding a TOML
  doc always yields exactly one row, so `encode(decode(x))` is faithful — but `encode` of a *multi-row* batch
  (e.g. produced by a query) silently drops rows 2..n. That is the only sensible behavior for a single-table
  format, and it is documented in the module header; no change needed, but a driver author should know TOML/md
  are single-row sinks.
- **`fallback_schema` is presently an identity clone** (convert.rs:106) because `unify` is total today. The
  comment says so. Fine; it is a defensive placeholder that keeps `rows_to_batch` panic-free if `unify` ever
  gains a strict mode. No action.
- **CSV float `f.to_string()` on encode** can lose the exact lexical form of the input (e.g. `1.0` ↔ `1`),
  which is acceptable for the *semantic* round-trip the ticket promises; `csv_roundtrip_is_value_stable` guards
  the value level. Consistent with the documented non-byte-identity policy.

## Cross-cutting coherence

The codec layer composes cleanly with the t05 type model and the t13 driver contract: codecs speak the one
canonical `RowBatch`/`Schema`, return the one `CfsError` (with new structured `Decode`/`Encode`/`UnknownCodec`
arms, each with a stable `code()`), and the registry is the third sibling of the mount/proc registries. The
acyclic spine is preserved — `qfs-codec → qfs-driver` (shared error) and `qfs-codec → qfs-types` are the two
declared, RFD-justified edges; no new back-edge. ARCHITECTURE.md's crate map and boundary rules remain accurate.

## Summary of carry-overs for the lead

1. **Nested-struct field-name loss (E4 / t05 follow-up) — the load-bearing one.** Decoded `Value::Struct`
   carries no names; `a.b.c` into a decoded nested object resolves positionally (`"0","1",…`) and will **not**
   match real keys, and the current tests do not exercise the decode→access path. Decide proposal (a) schema-in-
   struct or (b) static-type-threaded access, and add a decode→`access_row` test, before any driver ships
   `SELECT a.b.c` over decoded blobs.
2. **`infer_schema` / DESCRIBE-without-materialization (driver / DESCRIBE ticket).** The ticket-sketched
   `infer_schema` was folded into `decode`; record that `DESCRIBE` currently full-decodes, and add a defaulted
   `Codec::infer_schema` when a large-blob `DESCRIBE` needs a sampled schema (non-breaking to add later).

Neither blocks t15. Both are deliberate-deferral seams that the E4 driver tickets build on top of, recorded
now so the nested-data contract is unambiguous when t16/t18/t26 consume it.
