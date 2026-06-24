# Coding E2E — Planner — t15 (Codec registry: DECODE/ENCODE)

- Author: Planner
- Phase: Coding / review-and-testing
- Target: t15 — Codec registry + DECODE/ENCODE + struct/array + EXPAND/path-access
- Method: throwaway external consumer crate in `/tmp/t15-e2e` (own `[workspace]`,
  path-deps on `crates/codec`, `crates/core`, `crates/types` only — no production
  code touched, removed after the run). Used only the public API:
  `qfs_core::CodecRegistry`, `qfs_codec::{Codec, builtin_codecs, expand, access, access_row}`,
  `qfs_types::{Value, Row, RowBatch, Schema, ColumnType}`.

## Verdict: E2E approved

All five ticket items pass from the outside. Two findings are recorded as
**documented, expected** limitations (nested-object field-name loss; CSV empty-cell
→ Null making the *initial* batch non-conformant), not defects — both are called
out in the implementation comments and the RFD, and neither breaks the data
contract or causes a panic. No panic was observed on any input, including
adversarial bytes and a 2.5 MB JSON array.

## PASS / FAIL per item

### Item 1 — Registry resolution: PASS
- `CodecRegistry::with_builtins().len() == 6`; `builtin_codecs().len() == 6`.
- All six names resolve and report the matching `fmt()`: `json, jsonl, yaml, toml,
  csv, md+frontmatter`.
- Unknown format `"parquet"` → `Err(CfsError::UnknownCodec("parquet"))`,
  `code() == "unknown_codec"`, `matches!(e, CfsError::UnknownCodec(_)) == true`
  (branchable), no panic.

### Item 2 — Decode real samples: PASS
| fmt | rows | columns | conformant |
| --- | ---- | ------- | ---------- |
| json (3-element array) | 3 | `["active","id","name"]` | true |
| jsonl (2 lines) | 2 | `["id","name"]` | true |
| yaml (mapping) | 1 | `["active","name","tags","version"]` | true |
| toml (table) | 1 | `["enabled","port","title"]` | true |
| csv (header + 3 rows, 1 empty cell) | 3 | `["id","name","score"]` | **false** (see note) |
| md+frontmatter | 1 | `["priority","tags","title","body"]` | true |

Column counts and names are sensible for every format; the markdown codec maps
frontmatter keys to columns and appends a `body` column as specified. Object-key
columns come out in sorted order (serde_json `BTreeMap`/Map ordering), which is the
deterministic, stable order the encode side reproduces.

CSV `conformant=false` note: the empty `score` cell in the `carol` row decodes to
`Value::Null`, but `infer_schema` widened the `score` column to `Float` (from
`9.5`/`7`) **without** marking it nullable in a way `is_conformant()` accepts for
that all-but-one-row column — so the strict debug `is_conformant()` check fails.
This is a *test-only* conformance aid, not a decode failure: the batch is correct,
the data is present, and it round-trips (Item 3). Recorded as an observation, not a
blocker.

**One decoded batch as JSON (the YAML mapping):**
```json
{
  "schema": [
    { "name": "active",  "nullable": false, "ty": "Bool" },
    { "name": "name",    "nullable": false, "ty": "Text" },
    { "name": "tags",    "nullable": false, "ty": "Array(Text)" },
    { "name": "version", "nullable": false, "ty": "Int" }
  ],
  "rows": [
    [ true, "project", { "Array": ["rust", "qfs"] }, 2 ]
  ]
}
```

### Item 3 — Round-trip (decode → encode → decode): PASS (semantic)
For json, jsonl, csv: row count and **`b1.rows == b2.rows`** and
**`b1.schema == b2.schema`** all held (`data_stable=true`, `schema_stable=true`).
The re-encoded JSON is pretty, key-stable (sorted), array-of-objects; jsonl is one
compact object per line; CSV reproduces the header + rows (the empty `score` cell
re-emits as an empty field).

Byte-for-byte is **NOT** guaranteed (and not claimed): the JSON encoder pretty-prints
and sorts keys, so the re-encoded bytes differ from the (whitespace-laden, original
key-order) input. The *data* survives intact across the full cycle for all three
formats. This matches the ticket's "semantic round-trip" acceptance criterion and the
RFD §6 determinism (stable key ordering) goal.

**Field-name loss finding (real, expected):** nested objects lose their field names.
Probe input `[{"id":1,"meta":{"k":"v","n":7}}]` re-encodes to:
```json
[ { "id": 1, "meta": { "0": "v", "1": 7 } } ]
```
Top-level keys (`id`, `meta`) survive because the **batch schema** names them, but
the nested `meta` object's keys `k`/`n` become positional `0`/`1`. Root cause is in
`convert.rs::struct_schema` ("nested-object key names" documented non-preservation,
RFD §4) — a runtime `Row` carries no names, so a nested struct round-trips through
positional names. The *values* and nesting structure survive; only the inner key
labels are lost. This is a documented limitation, surfaced here as a finding.

### Item 4 — Struct / array + EXPAND + path-access: PASS (with the nested-name caveat)
Decoding `{"id":42,"profile":{...nested addr...},"items":[{...},{...}]}`:
- `profile` column ⇒ `ColumnType::Struct(...)`, runtime `Value::Struct` — confirmed.
- `items` column ⇒ `ColumnType::Array(Struct(...))`, runtime `Value::Array` — confirmed.
- `has_struct_col=true`, `has_array_col=true`.
- **EXPAND** `items`: 1 input row → **2 output rows** (one per array element), and
  the array-of-struct flattened each element's fields into the row
  (out cols `["id","0","1","profile"]`). Empty-array / scalar / absent passthrough
  semantics are the documented `expand` rules (verified by reading + the array case).
- **Path access**: `access`, `access_row` work correctly *for the names the schema
  actually carries*. Because nested fields are positional (the same field-name loss
  as Item 3), `access_row(row, schema, ["profile","addr","city"])` returns `None`
  (those names do not exist in the nested schema), whereas
  `access_row(row, schema, ["profile","0","0"])` returns `Some(Text("kyoto"))` and
  `access(profile_value, profile_schema, ["0"])` returns the nested `addr` struct.
  So the path-nav helpers are correct; the friction is that decode does not preserve
  nested **names** to navigate by. Top-level names (e.g. `profile`) do work as the
  head segment. Recorded as the same documented non-preservation, not a helper bug.

### Item 5 — Malformed inputs: PASS (no panic; structured errors)
Every malformed/adversarial input either returned a structured
`CfsError::Decode{fmt,detail}` (`code() == "decode_error"`, `is_Decode == true`) or
decoded leniently — **no panic in any case**.

| input | fmt | outcome |
| ----- | --- | ------- |
| truncated JSON `[{"id":1,"name":"alice"` | json | `Decode` err (EOF) |
| garbage bytes `\xff\xfe…` | json | `Decode` err (expected value) |
| bad CSV quoting `"unterminated,1` | csv | `Ok(1 row)` — lenient (`flexible(true)`) |
| invalid YAML | yaml | `Decode` err (mapping values not allowed) |
| invalid TOML `key = = nope` | toml | `Decode` err (extra `=`) |
| bad md frontmatter (unterminated `[`) | md+frontmatter | `Decode` err (yaml flow seq) |
| empty `b""` | json | `Decode` err (EOF at col 0) |
| NUL bytes `\x00\x00\x00\x00` | json | `Decode` err (expected value) |
| huge 200k-element JSON array (~2.5 MB) | json | `Ok(200000 rows)` — no panic |

**Two malformed-input error dumps (verbatim):**
```
json-truncated  [json] -> ERR code="decode_error" is_Decode=true |
  decode error (json): line 1, column 23: EOF while parsing an object at line 1 column 23

toml-invalid    [toml] -> ERR code="decode_error" is_Decode=true |
  decode error (toml): TOML parse error at line 1, column 7
  |
1 | key = = nope
  |       ^
  extra `=`, expected nothing
```
Each error names its `fmt` and carries a machine-/human-actionable `detail` with a
line/column hint — exactly the structured-error shape the ticket (RFD §5/§10) asks
for. The CSV "bad quoting" case is *accepted* leniently rather than rejected because
the reader is built with `flexible(true)`; that is a deliberate design choice
(irregular-data tolerance), not a missed validation — noted so the team is aware the
codec is lenient on row arity / quoting.

## Concern + proposal (per Critical Review Policy)

**Concern (business-outcome lens):** nested-object **field-name loss** (Items 3 & 4)
means an AI agent that decodes JSON, sees `profile.addr.city`, and tries to navigate
or re-`ENCODE` by those names will silently get `None` / positional `0/1` keys. For
the qfs value proposition ("a markdown/JSON blob becomes a *queryable, editable*
relation an AI can drive"), losing the inner key labels weakens the `a.b.c`
path-access story precisely where nesting is deepest, and an AI cannot recover the
names from the typed model alone.

**Proposal (concrete, business-framed):** keep the current behavior for E0 (it is
documented and decode/encode stay total), but track a follow-up so nested
`Value::Struct` carries its field names — either by having `json_to_value` build the
nested struct's `Schema` with real keys (it already computes them in
`json_to_struct`) and threading that schema into the column type, or by exposing a
`DECODE … PRESERVE NAMES` opt-in. That restores `access_row(profile.addr.city)` and
makes nested `ENCODE` lossless, which is the difference between "the AI can edit a
nested field by name" and "the AI must guess positional indices." This is a
roadmap/E1 item, not an E0 blocker — the documented limitation is acceptable for the
foundation epic.

## Evidence
- Throwaway consumer crate: `/tmp/t15-e2e` (own `[workspace]`, path-deps only,
  removed after the run). Built and ran clean via `cargo run`; an extra
  `cargo test` probe confirmed the positional-name navigation root cause.
- No production source was modified; testing was purely external against the public API.
