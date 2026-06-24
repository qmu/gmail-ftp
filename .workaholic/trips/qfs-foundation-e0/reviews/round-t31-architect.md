# Round t31 — Architect Analytical Review

Author: Architect (Neutral / Structural)
Reviewed: Constructor commit `a80dad3` — t31 server binding DDL
Phase: coding / review-and-testing
Scope: analytical (code + architectural + model checking; no test execution)

## Decision: APPROVE WITH OBSERVATIONS

t31 is structurally faithful to ticket intent and, on the one carry-over I personally
flagged at t30 (CO-t30-2/3), delivers a *stronger* closure than the renderer I proposed:
a round-trippable serde-canonical spec rather than a stable Debug key. The three headline
governance questions all resolve in the Constructor's favor. Observations below are
seams/carve-outs to track, not blockers.

---

## Headline ruling 1 — `AT`/method-route vs `ON` keyword-freeze: CORRECT governance call

**Ruling: the no-new-closed-core-keywords invariant correctly OUTRANKS the ticket's
illustrative `AT`/method-route surface syntax. The deferred `AT` keyword-freeze change is
correctly framed out-of-scope.**

Verified against the actual frozen set in `crates/lang/src/keywords.rs`:
- `ON` (line 110/156/210), `ENDPOINT`, `WEBHOOK`, `DO`, `EVERY`, `AS` are all present in
  the RFD-0001 §3 verbatim-transcribed frozen table.
- `AT` is **genuinely absent** (`grep "AT"` → no match; exit 1). There is no `Keyword::At`.
  Adding `AT` would mean editing the frozen golden fixture *and* the freeze test that locks
  it — i.e. a keyword-freeze RFD change, exactly the closed-core governance act t04 committed.

The ticket's literal grammar (`AT <route>`, bare `<method> <route>`) is **illustrative
surface syntax**, not a frozen contract; the hard ticket invariant ("No new keywords beyond
the reserved set… the DDL is pure sugar", Overview L20 + Acceptance L134) and the project's
keyword-freeze discipline are the binding constraint. With t04 having frozen `ON` and **no
in-repo RFD to override it**, binding over `ON` is the only call that honors both the
ticket's own invariant and the committed freeze. The Constructor adding zero keywords and
modeling method/route/event as typed DTO values riding the `ON` operand is the
structurally correct resolution.

**Expressiveness check (does binding-over-`ON` lose anything?):** No ambiguity is
introduced. `split_method_route` (server.rs L432) splits the `ON '<method> /route>'` operand
on first whitespace → `(HttpMethod, Route)`, with a bare `/route` defaulting to `GET`. Every
binding remains expressible: endpoint method+route, webhook route, trigger event, job
interval (`EVERY`), bodies (`AS`/`DO`) each have a distinct typed slot. The method authority
is the route table, not the grammar (`HttpMethod::Other` keeps an unrecognized method
verbatim rather than rejecting) — a clean separation. The compromise is purely *ergonomic*
(`ON 'GET /recent'` vs `AT GET /recent`), not *semantic*; nothing becomes inexpressible.

**Confirmed:** the deferred `AT`/explicit-method-route grammar is correctly out-of-scope and
belongs in a separate closed-core keyword-freeze ticket (it is a frozen-set edit, not a
desugar concern). I endorse that framing.

*Observation O1 (translation-fidelity, non-blocking):* the divergence between the ticket's
literal grammar and the implemented `ON` surface should be reconciled in the ticket/RFD text
so a future reader does not treat `AT` as live. The code documents this thoroughly (server.rs
L12-17, L154-156, L217-218; tests.rs L86-88), but the *ticket* still reads `AT`. Proposal:
the Lead routes a one-line note to the ticket (or the deferred freeze ticket) recording that
`AT` is the chosen future keyword and `ON` is the t31-shipped surface, so the carry-over is
traceable. This is a paper-trail fix, not a code change.

---

## Headline ruling 2 — deferred-body spec + CO-t30-2/3 closure: GENUINELY CLOSED

**Ruling: the CREATE ≡ INSERT body-bearing equivalence is genuinely closed (structural, not
a superficial string match). The purity invariant holds.**

This is the carry-over I authored at t30 (`round-t30-architect.md` L147-159, L281-293):
the body-bearing equivalence rested on an *unstable AST `Debug` projection* used as a
config-row key, leaving body-bearing CREATE/INSERT equivalence unproven. t31 replaces that
with serde-canonical JSON over the owned, span-normalized AST. I judge this **stronger than
the canonical-statement-renderer** I proposed in CO-t30-2: a renderer produces a *stable
string*; serde produces a *round-trippable value* the runtime can rehydrate with no parser
(spec.rs L62-70 `from_canonical`). The body-storage gap is closed at a better point.

**(a) Span-normalization soundness — SOUND.** Spans in this AST are byte-offset
diagnostic metadata only (`serialize_span` / new `deserialize_span` in ast.rs map a `Span`
to/from `[u32;2]`; `Span` carries no semantic payload). `normalize_spans` (spec.rs L132-281)
zeroes every span across the full AST surface — I traced the walker and it covers every
span-bearing node: `PlanWrap.span`, `PathExpr.span`, `Codec.span`, `FnRef.span`,
`CallRef.span`, and recurses through `Pipeline`/`Source`/`PipeOp` (incl. `Join`,
`Union`/`Except`/`Intersect`, `Decode`/`Encode`, `Call`), `EffectBody` (`Values`,
`Pipeline`, `SetWhere`), `returning` projections, and every `Expr` arm (`Binary`, `Unary`,
`In`/`AnyOp`, `Between`, `Like`, `Fn`). The no-span variants (`Limit`, `Distinct`, `As`,
`Expand`, `Path`/`Col`/`Lit`) are correctly left alone. Zeroing loses no semantics because
spans are purely for error reporting — two structurally identical bodies parsed at different
offsets converge to one byte-identical canonical form. Sound.

**(b) INSERT-string-column → spec parity — HOLDS across realistic bodies.** `lower_effect`
(lower.rs L200-218 `normalize_body_column`) parses the `plan`/`query` STRING column with the
**same** `parse_statement` the CREATE path uses, then canonicalizes via the **same**
`PlanSpec`/`StatementSpec::from_statement`. Because both origins run the identical parser and
the identical span-normalizing serializer, the canonical strings are byte-identical for any
body the parser accepts — predicates (`REMOVE /tmp WHERE age > 7`), nested pipes, joins, all
covered by the recursive normalizer in (a). The golden in tests.rs L186-206
(`body_bearing_create_equals_its_insert_twin_via_canonical_spec`) and the e2e scenario-5
flip exercise exactly this. Structural, not superficial.

**(c) Non-parseable INSERT body kept verbatim — SOUND carve-out, NOT a hole.** `Err(_)` in
`normalize_body_column` (lower.rs L214) keeps a non-qfs `plan`/`query` string literally. I
judge this sound, with a bounded scope argument:
  - The CREATE path *cannot* produce a non-parseable body — `from_server_ddl` only ever
    wraps an already-parsed `Statement` (the t04 grammar parsed it), so a malformed `DO`/`AS`
    is rejected at CREATE time (tests.rs L158-167). The verbatim path is reachable **only**
    via an explicit hand-written `INSERT INTO /server/…` carrying a non-qfs marker string.
  - For that explicit path, rejecting would *regress* a legitimate capability (an operator
    inserting an opaque marker). Keeping it verbatim preserves the explicit-write contract.
  - The carve-out does **not** weaken the equivalence claim: equivalence is asserted between
    a CREATE body and an INSERT twin carrying *the same genuine qfs source*. A non-qfs marker
    has no CREATE twin to be unequal to, so there is nothing to break.
  This is a carve-out at the right boundary (explicit-write vs sugar), not a hole where a
  malformed *binding* slips through. See O2 for the one residual.

**Purity invariant — HOLDS.** Bodies are stored as `PlanSpec`/`StatementSpec` = serialized
**data**, never a live `qfs_plan::Plan`. `PlanSpec` is a newtype over `StatementSpec`
(spec.rs L76-120) precisely to type the body's *role* (a plan to run later) without making it
executable. The desugar path (`desugar_to_insert` → `server_write_plan`, server.rs L595-588)
builds a `Plan` but runs no I/O — the COMMIT-time `apply_server_write` is the only impure
step. Confirmed: embedding a `DO <plan>` does not execute it.

*Observation O2 (structural, non-blocking):* the verbatim carve-out means the *stored*
`plan`/`query` value is heterogeneous — usually canonical spec JSON, occasionally an opaque
non-qfs string. The runtime's fire-time `from_canonical` (E7) will `Err` on the opaque case.
That is correct (an opaque marker has no fire semantics), but it is an *untyped* distinction:
nothing at the type level tells E7 "this is a spec" vs "this is a marker". Proposal: when E7
lands the fire path, have it treat a `from_canonical` `Err` on a stored body as a structured
"non-executable binding body" diagnostic rather than a parse-error surprise, and add an
e2e/golden asserting an opaque-marker INSERT round-trips verbatim and is *not* misreported as
corrupt. No t31 code change; a tracked seam for E7.

---

## Headline ruling 3 — tripwire flip (`assert_ne!`→`assert_eq!`) in Planner's e2e_serve.rs: LEGITIMATE

**Ruling: the cross-agent edit is LEGITIMATE. The Planner MUST independently re-confirm the
equivalence in their E2E pass (the flip is a premise inversion, and re-validation is the
Planner's QA domain regardless).**

The trip-protocol rule is "never modify another agent's *artifact*" (directions/models/
designs/reviews — the deliberative record). `e2e_serve.rs` is a **test fixture in the
codebase**, the Constructor's QA-domain output surface, not a Planner deliberative artifact;
the line is finer than "the Planner wrote it." Weighing the two competing harms:
  - **Leaving it red:** the tripwire's premise was "the gap is PRESENT" (`assert_ne!` +
    asserts the CREATE body contains `EffectStmt`/`Remove` Debug text and the INSERT body is
    the literal `REMOVE /tmp WHERE age > 7`). t31 *closed* that gap by design — the body is
    now canonical spec JSON, not Debug text, and both paths converge. The old assertion is now
    **knowingly false**. Shipping a knowingly-red test is itself a protocol harm (a broken
    gate that masks real regressions).
  - **The Constructor flipping it:** the tripwire was explicitly authored as "a future fix
    that closes the gap makes this test fail loudly… not silently regress" (old comment,
    L+probe). t31 *is* that fix. The Constructor closing the loop the tripwire was designed
    to catch — and re-pointing it at the achieved equivalence — is the tripwire working as
    intended, not a boundary violation. The flip is mechanical and faithful: `assert_ne!`→
    `assert_eq!`, and the now-stale "INSERT body is the literal string" / "Debug projection"
    assertions are replaced with "stored body is the canonical serialized spec".

So the edit is legitimate as a *Constructor QA-surface change that closes its own predicted
tripwire*. **However**, the equivalence is precisely a Planner E2E concern (CREATE ≡ INSERT
end-to-end through boot/apply/stored-row), and the Planner is the QA owner of e2e_serve.rs's
*meaning*. **The Planner MUST independently re-run and re-confirm scenario 5** in their E2E
pass — not merely accept the Constructor's flip on faith. If the Planner agrees the
equivalence holds end-to-end, they ratify; if they find a residual, they own the re-flip.
This keeps the artifact-ownership spirit (the Planner validates their own gate) while not
shipping a knowingly-red test in the interim.

*Observation O3 (process, non-blocking):* the cleanest future pattern is for the Constructor
to flag a cross-fixture tripwire flip in the commit body (it did: the commit subject names
"closing the t30 body-storage gap") and for the Lead to route the Planner a re-confirm
directive (this review does). Proposal: the Lead's E2E hand-off to the Planner should
explicitly list scenario 5 as "re-validate the flipped tripwire," making the ratification an
intentional gate rather than an implicit one.

---

## Other surfaces

**S1 — placement in `qfs-core::ddl::server`: CORRECT, confinement green.** The DDL is
closed-core (frozen keywords, shared across backends), so it belongs in core, not a driver —
ticket-intent-aligned (Key components L56-57; Considerations L120-121). `qfs-server`'s
`lower.rs` is now a thin adapter (routes `CREATE` to core's `from_server_ddl`/`desugar`,
normalizes the explicit INSERT twin) and re-exports the core primitives so its public surface
is unchanged (lib.rs L51-53). `server_node_schema` is now owned by core and re-exported by the
driver (driver.rs), eliminating the DESCRIBE-vs-desugar drift risk — one source of truth.
Confinement verified: `grep winnow|tokio|cloudflare|reqwest|worker crates/core/src/ddl/` →
**no matches**. Core's ddl imports only `qfs_parser` (owned AST, already a wired edge),
`qfs_plan`, `qfs_types`, `serde`/`serde_json`. No vendor/SDK/async leak into closed core.

**S2 — `serde::Deserialize` on the parser AST + `deserialize_span`: SOUND, owned-only.** The
new `deserialize_span` (ast.rs L47-58) is the inverse of `serialize_span` (`[u32;2]`↔`Span`),
making the AST fully round-trippable. `qfs_lang::Span` stays serde-free (the lexer crate
keeps zero-dep) — the AST owns the projection, no vendor leak. All derived `Deserialize`
adds are on owned types (`Statement`, `Pipeline`, `Expr`, `Op`, `Literal`, etc.). The
round-trip is exercised in tests.rs L98-112. The closed `Statement`/`PipeOp` variant sets
remain locked by the governance test (the derive does not loosen `#[non_exhaustive]`-style
governance).

**S3 — five forms + MATERIALIZED desugar to one INSERT: CORRECT.** Each `ServerBindingDdl`
variant maps to exactly one `ServerNode` (server.rs L248-256) and `desugar_to_insert` builds
a single-node `Plan` with `Affected::Exact(1)` (L595-599, L576-588). `materialized` bool is
set from `DdlKind::MaterializedView` (L394) and asserted both ways (tests.rs L67-82).
Parse-time rejections are structured, no panic: malformed body → `DdlError::Parse`
(tests L158-167), unknown column → `UNKNOWN_COLUMN` (`config_row_batch` L551-571, tests
L176-183), unknown subkeyword `POLICY` → `UNSUPPORTED_DDL` (L407-410, tests L150-156),
missing clause → `MISSING_CLAUSE` (tests L169-174). `DdlError::code()` is stable for the AI
self-correction path; `ParseError.code` confirmed present (error.rs L66).

**S4 — UPSERT-by-name idempotency coherent with t30: CONFIRMED.** `CREATE_WRITE_OP =
ServerWriteOp::Upsert` (server.rs L484); both `lower_ddl` (lower.rs L86) and core's
`desugar_to_insert` use it. The doc-comment (L478-483) correctly records: CREATE is
declarative "make this exist" → UPSERT so a `config.qfs` replay/boot converges (RFD §6,
coherent with t30's UPSERT choice), while an explicit `INSERT INTO /server/…` keeps
fail-on-duplicate via `EffectVerb::Insert → ServerWriteOp::Insert` (lower.rs L126). The two
verbs are distinct and the carve-out is documented. Coherent.

**S5 — `policy_ref` seam + `WHERE`-on-TRIGGER park: ACCEPTABLE.** `PolicyRef =
Option<String>` is present on all five decl DTOs (server.rs L58, L170/186/200/214/226),
`None` until t34, stored as data never a token — a clean no-migration seam (Considerations
L102-105). The trigger predicate seam is a typed `Option<StatementSpec>` left `None` because
**t04 does not separately surface a standalone `WHERE <pred>` clause** on TRIGGER — the t04
`ServerDdl` carries no predicate field; the `WHERE` in `DO REMOVE /tmp WHERE age > 7` is part
of the `DO` body, not a trigger guard (server.rs L369-372). This is an honest park: the seam
exists, typed and parse-now-ready, for when t04 surfaces it. Acceptable. *Observation O4:* the
`from_server_ddl` comment is slightly imprecise — it says the predicate "is carried in t04 as
part of the DO body shape," which is true for the *body's* WHERE but means a *trigger-guard*
WHERE is not yet expressible at all. Proposal: when the guard is needed, the surfacing is a
t04 grammar change (a new optional clause), tracked as a seam dependency — note it so t34/t33
does not assume the guard already round-trips. Paper-trail only.

**S6 — `ServerState` stores canonical spec JSON, rehydrates via `from_canonical`:
CONFIRMED, coherent with t30.** `StatementSource(pub String)` (state.rs L27) is unchanged in
*type*, but its *content* is now canonical spec JSON (the column value `lower_effect`/the
core desugar produce), not the t30 Debug projection. The driver's apply path
(`apply_server_write`, driver.rs L182+) maps the row's `plan`/`query` column value verbatim
into `StatementSource::new(text("plan"))` (driver.rs L309/319/333/344) — it does **not**
re-parse at apply time. Rehydration via `StatementSpec::from_canonical` / `PlanSpec::
from_canonical` (spec.rs L62-70, L111-119) is the runtime's **fire-time** (E7) path and runs
**no parser**, so the runtime cannot hit a parse error at fire time — exactly the property the
ticket's "hard part" requires (Considerations L112-116). Keeping `StatementSource` as a string
preserves `ServerState`'s `Serialize`/`Deserialize` and snapshot stability (state.rs L21-25).
No remaining Debug-projection producer anywhere (`grep` for `{:?}` body projection → none in
server/core except an unrelated secret-hygiene assertion). Coherent with t30.

---

## Cross-cutting coherence

The t31 change is a clean structural bridge: the frozen surface (`ON`/`AS`/`DO`/`EVERY`)
maps through owned typed DTOs to one canonical config row to one `/server` INSERT plan, with
the deferred body as round-trippable data — the closed-core thesis holds end-to-end and the
purity invariant is preserved at every hop. The single carry-over I owned (CO-t30-2/3) is
closed at a stronger point than I proposed. No structural concern blocks acceptance.

## Carry-overs (for the Lead to route)
- **CO-t31-1 (ticket/RFD paper-trail):** reconcile the ticket's literal `AT`/method-route
  grammar with the shipped `ON` surface; record `AT` as the future keyword-freeze ticket.
- **CO-t31-2 (E7 fire path):** treat a `from_canonical` `Err` on a stored body as a
  structured "non-executable body" diagnostic; add a golden that an opaque-marker INSERT
  round-trips verbatim and is not misreported as corrupt (the verbatim carve-out, O2).
- **CO-t31-3 (Planner E2E gate):** the Planner MUST re-confirm scenario 5 (the flipped
  body-bearing equivalence tripwire) independently in their E2E pass before ratifying.
- **CO-t31-4 (t04 seam):** the trigger-guard `WHERE` is a future t04 grammar surfacing; the
  typed `None` seam is parked but does not yet round-trip (O4) — note for t33/t34.

## Review Notes
Analytical review only; no tests/build/clippy executed (Architect QA domain). Internal
testing is the Constructor's domain; E2E (incl. the scenario-5 re-confirm) is the Planner's.
