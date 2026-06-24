---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash: d2f116e
category: Added
depends_on: [20260622214650-t05-type-schema-model.md]
---

# Codec registry: DECODE/ENCODE + struct/array model

## Overview

This ticket implements the **codec registry** — the third of the three open
registries that keep the cfs grammar closed while letting capabilities grow
(RFD §3). Codecs bridge the blob↔relational boundary: `DECODE fmt` turns any
opaque byte blob (from FS, S3, git, Drive, or a REST response) into typed rows;
`ENCODE fmt` does the reverse (RFD §4). With codecs, a markdown file with YAML
frontmatter becomes a queryable/editable table (`frontmatter keys = columns`,
`body = content`), so `.workaholic/**/*.md` is itself a relation.

It also delivers the **struct/array column model** and the `EXPAND` /
path-access (`a.b.c`) semantics described in RFD §4 — the nested-data layer that
the codecs produce and that the query side consumes. Codecs are **pure**
`bytes↔rows` transforms (purity invariant, RFD §3); they never perform I/O and
work on **any** blob source, so the same `DECODE json |> EXPAND items` pipeline
runs over a local file, an S3 object, or a webhook body unchanged.

## Scope

In scope:
- `Codec` trait + a name-keyed `CodecRegistry` (json, jsonl, yaml, toml, csv,
  markdown+frontmatter).
- `DECODE`/`ENCODE` evaluation against an in-memory `Bytes`→`Vec<Row>` boundary.
- Struct/array `Value`/`ColumnType` extensions and inferred schema from decoded data.
- `EXPAND <field>` operator semantics + `a.b.c` path access over struct columns.
- Markdown+frontmatter ↔ row mapping (frontmatter cols + `body`).

Out of scope (deferred):
- Reading bytes *from* a service — codecs take/return `Bytes`; sourcing them is
  the driver/VFS layer (E4 driver tickets) and the `FROM <path>` resolution.
- Pushdown of decode into a remote engine (e.g. DuckDB read_json) — runtime
  federation ticket (E2).
- Parser/grammar wiring of the `DECODE`/`ENCODE`/`EXPAND` keywords into the AST
  is owned by the language-core ticket (E1); this ticket provides the evaluable
  registry + operator semantics those nodes dispatch to.
- Binary/columnar formats (parquet, avro) — future codec tickets.

## Key components

New crate/module `cfs-codec` (Domain layer), depending only on `cfs-type`
(t05) — no driver/vendor types leak in (owned DTOs, RFD §9).

```rust
// Pure bytes <-> rows. No I/O, no async, no driver awareness.
pub trait Codec: Send + Sync {
    fn name(&self) -> &'static str;
    fn decode(&self, bytes: &[u8], opts: &DecodeOpts) -> Result<RowSet, CodecError>;
    fn encode(&self, rows: &RowSet, opts: &EncodeOpts) -> Result<Vec<u8>, CodecError>;
    /// Best-effort schema without materializing all rows (powers DESCRIBE).
    fn infer_schema(&self, bytes: &[u8], opts: &DecodeOpts) -> Result<Schema, CodecError>;
}

pub struct CodecRegistry { /* name -> Arc<dyn Codec> */ }
impl CodecRegistry {
    pub fn with_builtins() -> Self;            // json, jsonl, yaml, toml, csv, md
    pub fn register(&mut self, c: Arc<dyn Codec>);
    pub fn get(&self, name: &str) -> Result<Arc<dyn Codec>, CodecError>; // structured "unknown codec"
}
```

- `RowSet` / `Row` / `Value` (from t05) extended with `Value::Struct(BTreeMap<String,Value>)`
  and `Value::Array(Vec<Value>)`; `ColumnType::Struct(Schema)` / `ColumnType::Array(Box<ColumnType>)`.
  Irregular JSON collapses to `Value::Json`/opaque struct rather than failing.
- Builtin codecs: `JsonCodec`, `JsonlCodec`, `YamlCodec`, `TomlCodec`, `CsvCodec`,
  `MarkdownFrontmatterCodec` (frontmatter → columns, remainder → `body: text`).
- `expand(rows, field) -> RowSet` and `access(value, path: &[&str]) -> &Value`
  implementing `EXPAND` and `a.b.c` (RFD §4). One `EXPAND` serves mail
  attachments and JSON arrays alike.
- `CodecError` (thiserror): `UnknownCodec`, `Decode{fmt,detail}`, `Encode{..}`,
  `SchemaMismatch` — structured so the AI gets a typed parse/eval error (RFD §10).

## Implementation steps

1. Extend t05 `Value`/`ColumnType`/`Schema` with `Struct`/`Array` variants; add
   constructors and `Display`/serde round-trips. Keep `Json` as the irregular fallback.
2. Define the `Codec` trait, `DecodeOpts`/`EncodeOpts`, `CodecError`.
3. Implement `JsonCodec` + `JsonlCodec` (serde_json): object→row, top-level array→
   row-per-element; nested objects→`Struct`, nested arrays→`Array`.
4. Implement `YamlCodec` (serde_yaml), `TomlCodec` (toml), `CsvCodec` (csv crate;
   header→columns, all-string unless type hint, configurable delimiter).
5. Implement `MarkdownFrontmatterCodec`: split `---`-delimited YAML frontmatter,
   decode it to columns, attach remaining text as `body`; encode reverses it.
6. Build `CodecRegistry::with_builtins()` + `register`/`get`.
7. Implement `expand` and path-access `access`; define EXPAND on absent/scalar
   fields (empty/passthrough) explicitly.
8. Implement `infer_schema` per codec (sample-based for jsonl/large json).
9. Round-trip property tests + golden fixtures; wire `CodecError` into the shared
   structured-error type.

## Considerations

- **Purity invariant (RFD §3):** codecs are `bytes↔rows`, no async, no I/O, no
  driver handles — this is what keeps `DECODE` dry-runnable and lets the same
  pipeline run over any source. Enforce by giving the trait no I/O surface.
- **No vendor leaks (RFD §9):** the registry exposes only owned DTOs (`RowSet`,
  `Value`); `serde_json::Value`/`toml::Value` stay internal to each codec impl.
- **Irregular data:** deeply irregular JSON must not crash decode — collapse to a
  `Struct`/`Json` column (RFD §4). Hardest part: schema inference over
  heterogeneous arrays — sample N rows, widen to a union/`Json` on conflict, and
  surface the inferred schema via `DESCRIBE`.
- **Round-trip fidelity:** `ENCODE(DECODE(x))` is not byte-identical (key order,
  whitespace, comments). Aim for *semantic* round-trip; document non-preservation
  (TOML/markdown comments) rather than promising byte equality.
- **Encode determinism (idempotency, RFD §6):** ENCODE must be deterministic
  (stable key ordering) so `UPSERT` of an encoded blob is retry-safe and diffs
  are stable.
- **Least privilege / secrets:** codecs touch no credentials; they receive only
  bytes already fetched under capability/POLICY gating upstream — keep that
  boundary clean (no path/URL awareness inside a codec).
- **Observability:** decode/encode errors carry `fmt` + byte offset/line where
  available for actionable logs.
- **Standards:** Domain crate, no `unsafe`, `#![deny(clippy::all)]`; one codec per
  module file under `cfs-codec/src/codecs/`.

## Acceptance criteria

- `cargo build`, `cargo clippy -- -D warnings`, `cargo test` green; no network,
  no live creds in any test.
- `CodecRegistry::with_builtins().get(name)` resolves all six codecs; unknown
  name returns a structured `CodecError::UnknownCodec` (not a panic).
- Golden tests: each format has a fixture decoded to an asserted `RowSet`;
  markdown+frontmatter fixture yields frontmatter columns + a `body` column.
- Property test: for json/jsonl/yaml/toml/csv, `decode → encode → decode` is
  value-stable (semantic round-trip) on the sample corpus.
- `EXPAND items` over a `Value::Array` column produces one row per element; `EXPAND`
  on a scalar/absent field behaves per the documented rule; `a.b.c` path access
  navigates nested structs without flattening — all asserted by test.
- `infer_schema` returns a schema consistent with full decode on regular inputs
  and a `Struct`/`Json` fallback on irregular inputs (asserted).
