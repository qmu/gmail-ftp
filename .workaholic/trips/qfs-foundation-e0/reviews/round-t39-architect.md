# Architect Analytical Review — t39 (AI operating procedure + agent skill)

- **Reviewer**: Architect (Neutral / structural bridge)
- **Ticket**: `.workaholic/tickets/todo/a-qmu-jp/20260622214650-t39-ai-operating-procedure-and-skill.md`
- **Commit reviewed**: `0b66f52`
- **Mode**: Analytical review only (no build / clippy / test execution — the Lead owns gate verification)

## Decision: **Approve with observations**

The structure faithfully delivers the ticket's thesis. The DESCRIBE→statement→PREVIEW→COMMIT loop
is genuinely uniform across all seven worked examples; the per-driver surface differences are
path-grammar / lowering surface, not loop exceptions. The `DescribeReport` DTO is owned-only and
cred/IO-free by construction; the `DescribeProvider` composition-root injection is a faithful
sibling of the t28/t32 launchers and keeps qfs-cmd off the driver crates. One genuine
intent-vs-acceptance gap (CO-t39-1: the skill does not yet ship discoverable from the running
binary) is adjudicated below as a recommended small fold-in, not a blocker.

---

## Headline 1 — Loop uniformity (the thesis). RULING: **uniform; no smuggled exceptions.**

I read SKILL.md, the seven `assets/examples/*.qfs`, and the golden corpus. Every example uses the
identical four steps: (1) DESCRIBE excerpt, (2) one qfs statement, (3) PREVIEW reading affected
count + `irreversible`, (4) COMMIT note. The corpus pins step 2/3 for each. The four candidate
"special cases" the Lead flagged are each **legitimate path/grammar/lowering surface**, NOT a loop
exception — and each is traceable to a genuine driver-contract declaration, not skill prose:

1. **`cp` → `UPSERT` lowering (drive).** This is a *shell-builtin desugar*, identical in kind to
   the `SEND` alias and `CREATE TRIGGER` desugar. SKILL.md and `drive.qfs` are explicit: `cp` is
   the human surface; the closed-core form it lowers to is `UPSERT INTO …` (the retry-safe blob
   write, RFD §6). The golden pins the lowered `UPSERT`, not a parallel `cp` codepath. The loop
   step (write a closed-core statement) is unchanged — `cp` is sugar over the same `Upsert`
   `EffectKind`. Not an exception.

2. **`/drive/my/...` prefix.** Pure addressing surface (the `my` drive root segment). The path is
   still an absolute `/driver/…` VFS address the DESCRIBE→statement loop consumes verbatim. Not a
   grammar or loop divergence.

3. **slack bare channel name (`/slack/acme/general/messages` in the golden vs `#general` in
   SKILL.md).** Verified against `crates/driver-slack/src/lib.rs`: the driver declares `ChannelRef`
   accepting both a bare name and `#name`, and `#name`→`Cxxxx` resolution is **explicitly the
   applier's commit-time I/O job — PREVIEW shows the symbolic channel** (purity invariant, RFD §3).
   So both forms are valid `ChannelRef` path surface and neither reaches an exception in the loop;
   the corpus deliberately uses the no-`#` form to keep the pre-parse addressing lexer simple. This
   is a *path-grammar* fact declared by the driver, not skill prose. **Minor observation below.**

4. **`SEND` alias (mail).** Verified in `crates/driver-gmail/src/lib.rs:135` —
   `AliasFn::new("SEND", "mail.send")` is a **real prelude alias the driver declares**, surfaced
   through `DescribeReport.aliases`, and the gmail test pins it. The agent reads it from DESCRIBE,
   not from special-cased skill text. This is exactly the contract-declared sugar the ticket
   *wants* (a pure prelude fn → `CALL`), the model case of "the loop reads only DESCRIBE."

**No driver-contract under-declaration surfaced.** Every per-driver surface difference is backed by
a t13 contract declaration (alias / archetype-hint / ChannelRef / EffectKind lowering) that flows
through `DescribeReport`, so the agent never needs prose the skill would have to special-case. The
negative golden (`UPDATE` on a slack append node → `unsupported_verb` at resolve time) closes the
loop honestly: an unsupported verb fails structurally *before* any plan, which is the mechanism that
makes "DESCRIBE is the only thing you read" enforceable rather than aspirational.

## Headline 2 — CO-t39-1 ship-discoverability gap. RULING: **recommend a small fold-in now.**

The facts: `qfs-skill` embeds `SKILL.md` via `include_str!` (`SKILL_MD` const, `EXAMPLES` manifest),
but **nothing links `qfs-skill` into the binary** — it is a `publish=false` dev/assets crate whose
only non-test consumer is… nothing. There is no `qfs skill [print]` subcommand. So the claim in
Key-components ("embedded so the loop docs ship inside the single binary", RFD §9) is **not true of
today's artifact**: the binary does not carry `SKILL_MD`.

Adjudication of the two readings:

- **Letter of acceptance**: the explicit ACCEPTANCE criteria require discoverability "from the docs
  index" (done — `docs/README.md` links `crates/skill/assets/SKILL.md`) and an RFD §11/E8 reference
  (done). Neither literally requires a runtime subcommand. By the strict letter, **acceptance is met
  and binary-embed is a design note.**
- **Intent**: the ticket's Overview and RFD §1 frame the whole point as *the agent learns the loop*;
  Key-components asserts the docs **ship inside the single binary** (RFD §9, "one binary"). An AI
  agent driving `qfs` discovers capabilities by running the binary (`qfs describe`, `qfs --help`),
  not by reading the repo's `docs/` tree. Under that intent, "discoverable from the running binary"
  is load-bearing, and a `qfs-skill` crate that no shipped artifact links is a const that exists only
  to satisfy its own unit test.

**Ruling**: I rule this a **recommended small fold-in, not a t40 carry-over** — but explicitly *not*
a blocker for archiving t39. The fold-in is genuinely small and closes the gap between the ticket's
stated Key-component (binary-embed) and the artifact:

- Add a `Skill { #[arg(...)] examples: bool }` subcommand to `qfs-cmd` that prints
  `qfs_skill::SKILL_MD` (and, with a flag, the `EXAMPLES` statements). This requires the binary to
  add a `qfs-skill` *normal* (non-dev) dependency — which is the edge that actually makes the embed
  claim true and which the dep guards already anticipate (`qfs-skill` is a terminal sink).
- Wiring is logic-free (mirror the `Describe`/`Serve` dispatch): qfs-cmd routes `Skill` to a thin
  print, the const lives in the leaf crate. No new C4/leaf risk (qfs-skill carries no runtime; its
  driver edges stay dev-only — see Surface 4).

If the Lead prefers to keep t39 strictly to its literal acceptance, then carrying CO-t39-1 to t40
(docs + distribution) is **defensible**, provided the Key-components "ships inside the single
binary" sentence is softened in the ticket/RFD to "authored to be embedded" so the record stops
overstating today's artifact. My recommendation is the fold-in: it is the smaller honesty cost and
it is the actual payoff of the epic.

## Headline 3 — DescribeProvider injection + driver describe-facet registration. CONFIRMED (a)(b)(c).

**(a) `DescribeReport::from_driver` folds only the introspective half — never `applier`.** Confirmed
by reading `crates/core/src/describe.rs:74-88`: it calls `describe` / `capabilities` / `procedures`
/ `prelude` / `pushdown` and constructs the report; `Driver::applier` is never named. The doc
comment states the purity invariant and the unit test `from_driver_folds_the_introspective_half`
exercises a `FixtureDriver` whose `NoopApplier` is present but never reached. The
`report_json_shape_is_stable` test asserts `!json.contains("token") && !json.contains("Bearer")`.
Describe is genuinely cred/IO/network-free.

**(b) Composition-root injection keeps qfs-cmd logic-free + off the driver crates.** Confirmed.
`crates/qfs/src/describe.rs::describe_registry()` builds the `MountRegistry` from the eight concrete
driver crates (cred-free `Mock*Client` / empty `ObjRegistry`) and `main.rs` injects it as the third
launcher arg. `qfs-cmd` only declares `DescribeProvider = dyn Fn() -> qfs_core::MountRegistry` and
`dispatch_describe` builds the registry via the provider then hands off to
`qfs_exec::run_describe` — no driver crate is named in qfs-cmd. The dep_direction guard's
`binary_is_the_thin_entrypoint_plus_the_t28_shell_composition_root` allowlist was extended (t39
block, lines 214-226) to admit the six new describe-facet driver edges *on the binary only*, with
the same terminal-sink rationale as t28/t32/t36. No C4 / leaf-confinement violation: qfs-cmd stays
on `qfs-core` + `qfs-server` only. This is a faithful structural sibling of `ShellLauncher`/
`ServeLauncher`.

**(c) sql/git/cf-d1 fallback is a registration gap, not a hidden credential need.** Confirmed and
honestly documented. `describe.rs`'s module doc and the golden corpus both state: sql/git/cf need a
registered *connection-catalog / repo / D1-catalog* for describe to resolve a concrete node — a
**registration** requirement (a catalog/repo object), not a credential. The golden corpus proves
this by building exactly those fixtures (`OfflineBackend` with a cached `orders` catalog;
`LooseObjectDb` empty repo) and describing through them with **no live backend** — the
`OfflineBackend::execute_read`/`commit_transaction` return an honest "offline fixture" error if ever
called, and they are not called on the describe path. So the binary's describe registry covers 8
cred-free drivers and the registration-gated three are covered by the corpus fixtures. The honesty
is sound: nothing claims a credential it doesn't need, and nothing hides a credential it does need.

---

## Other surfaces

1. **DescribeReport DTO** — Confirmed Serialize-only (no `Deserialize`, with a documented rationale:
   qfs never reads a report back). Reuses `qfs_types::Column` / `Capabilities` / `ProcSig` /
   `AliasFn` — no parallel types — plus the thin local `PushdownSummary` flattening
   `PushdownProfile` through its own `supports_*` accessors (cannot drift). `#[non_exhaustive]` on
   both structs. No vendor SDK type reachable. Formalizes the t13 `Driver::describe` hook cleanly.

2. **`qfs describe -json`** — The `-json` single-dash token normalization (`normalize_json_alias`)
   is a precise, well-scoped fix: it rewrites only the exact standalone `-json` token to `--json`,
   stops at the first `--`, and is documented as preventing clap from lexing it as bundled shorts
   (`-j -s -o -n`). Renders through the t29 output layer (`JsonRenderer`/`TableRenderer.describe`).
   Failure paths verified: unknown mount → `ErrorKind::Capability` → exit **3**; relative path →
   `validate_path` → `Usage` → exit **2** — both consistent with t29's one-kind-one-exit contract.
   **Observation**: `dispatch_describe` resolves format from `json || --format` else TTY/pipe
   default, but the `Describe` subcommand only carries `--format`, and the global `--json` is read
   from `cli.json` — fine, but the help text says "Default: table on TTY, json when piped" which is
   the same deterministic-pipe behavior as `qfs run`. Consistent. No concern.

3. **Golden corpus reuses the t38 qfs-test harness** — Confirmed: imports `qfs_test::{assert_plan,
   preview_handler}`, no parallel harness. `.no_io_performed()` / `.irreversible(0)` / `.snapshot()`
   assert plan, not side effects. No COMMIT, no network. The negative golden asserts
   `err.code() == "unsupported_verb"` at resolve time, before any plan. github split into a
   write-plan golden + a `CALL` resolution check (the `FROM…|>CALL` pipe is a pure Relation; the
   irreversible flag is asserted on the procedure contract) is an honest treatment of the read/effect
   distinction.

4. **qfs-skill confinement — exemption is narrow + sound. Confirmed.** The runtime-leaf guard
   (`runtime_is_confined_to_plan_and_types`) exempts `qfs-skill` (lines 424-430) with the exact right
   rationale: its edges onto the describe-facet driver crates are **dev-dependencies** (the golden
   corpus), which `cargo metadata` lumps into the generic view; a dev-dep compiles only for tests and
   can never transit tokio into a *shipped* artifact, and nothing depends on `qfs-skill` (terminal
   sink). Cargo.toml confirms: `[dependencies]` is empty; the driver/test/core edges are all under
   `[dev-dependencies]`. So the exemption opens **no runtime hole** — it is the same "tokio
   dead-ends here" property as the `qfs` binary, scoped to the dev-only sink. This is exactly why the
   embed-into-binary fold-in (Headline 2) stays safe: a *normal* `qfs-skill` dep would carry only the
   `&'static str` consts + `EXAMPLES` manifest (zero runtime deps in `[dependencies]`), never the
   dev driver edges. The fold-in does not reopen this hole.

5. **Goldens under `crates/test/tests/fixtures/` (CO-t39-3)** — Acceptable, mild locality smell. The
   five `plan_*_*.json` fixtures live with the t38 harness because `assert_plan(...).snapshot(name)`
   resolves the golden via the **harness crate's** `CARGO_MANIFEST_DIR`, not the consumer's. Putting
   them under `crates/skill/tests/fixtures/` would require the harness to learn the caller's manifest
   dir — a harness change out of t39's scope. So co-locating them with the harness is the structurally
   correct choice *given t38's resolution model*, at the cost of t39's goldens not living next to
   t39's corpus. **Proposal (CO-t39-3, low priority)**: a follow-up could teach the t38 harness a
   `snapshot_in(manifest_dir, name)` overload so each consumer crate owns its fixtures; until then,
   the shared-fixture-dir is the lesser evil and is honestly the harness's contract, not a t39 sloppiness.

---

## Concerns + proposals (Critical Review Policy — ≥1 per surface)

- **[Primary] CO-t39-1 binary-embed gap** → Fold in a thin `qfs skill [--examples]` subcommand
  printing `qfs_skill::SKILL_MD`, adding `qfs-skill` as a *normal* (zero-runtime-dep) binary
  dependency. Makes the "ships inside the single binary" claim true and gives the agent a
  run-from-binary discovery path. If deferred to t40, soften the ticket/RFD wording to stop
  overstating today's artifact.
- **[Minor] slack `#`-vs-bare path drift** → SKILL.md uses `/slack/acme/#general/messages`; the
  golden uses `/slack/acme/general/messages`. Both are valid `ChannelRef`, but the divergence could
  read as a smuggled exception to a careless agent. Proposal: add one line to `slack.qfs` /
  SKILL.md noting "`#general` and `general` are the same ChannelRef; PREVIEW shows the symbolic
  channel, the id resolves at COMMIT" — turning an apparent inconsistency into a documented
  path-grammar fact. Non-blocking.
- **[Minor] CO-t39-3 fixture locality** → see Surface 5; a `snapshot_in` harness overload is the
  clean fix, deferrable.

None of these block archiving. The loop is uniform, the DTO is clean, the injection is sound, and
the cred/IO-free invariant holds by construction.
