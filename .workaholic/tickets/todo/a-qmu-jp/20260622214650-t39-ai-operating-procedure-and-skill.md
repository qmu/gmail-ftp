---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [UX]
effort:
commit_hash:
category:
depends_on: [20260622214650-t29-cli-oneshot-and-output.md, 20260622214650-t30-server-runtime-and-self-config-driver.md]
---

# AI operating procedure + agent skill

## Overview

This ticket delivers the **single operating procedure** that an AI agent follows to
drive every service through `cfs`, and ships it as a discoverable `SKILL.md`. It is the
payoff of the whole architecture: per RFD §1, `cfs` "exists for AI" so an agent learns
*one* small grammar and *one* loop instead of N SDKs. The loop is:

> **DESCRIBE `<path>`** (discover archetype + schema + capabilities + procedures) →
> **write a cfs statement** (path-as-type, closed-core grammar) →
> **PREVIEW** (read the typed effect-plan + affected counts) →
> **COMMIT** (apply at the edge).

It implements RFD §2 (effect-plan / PREVIEW→COMMIT), §3 (closed core + three open
registries the agent must respect), §5 (driver contract is "everything the AI needs"),
and §7 (one-shot CLI surface the agent calls). It is documentation + a thin
`DESCRIBE`-output contract, **not** new engine semantics. The deliverable is a skill that
makes the uniform loop legible and a golden corpus proving the loop works identically
across mail/drive/github/slack/sql/git and server bindings.

## Scope

In scope:
- `SKILL.md` (agent skill) documenting the DESCRIBE → statement → PREVIEW → COMMIT loop,
  one procedure for every archetype, with worked examples per driver and per server binding.
- A stable, machine-readable **`DESCRIBE <path>` output contract** (JSON shape) the agent
  parses: archetype, columns+types, supported universal verbs, declared procedures, prelude
  aliases, pushdown summary. This formalizes what the driver contract already exposes.
- A `cfs describe <path> -json` one-shot subcommand wiring `DESCRIBE` to the CLI output layer.
- A golden example corpus (statements + expected PREVIEW plans) spanning all six drivers
  plus `/server/...` bindings, runnable with no live credentials.

Out of scope (deferred):
- The `DESCRIBE` engine internals / capability declaration mechanics — owned by
  `t13-driver-contract-trait.md`.
- One-shot output/`-json` formatting plumbing itself — owned by `t29-cli-oneshot-and-output.md`.
- Server binding DDL semantics — owned by `t31-server-binding-ddl.md`; we only *cite* it.
- Per-handler POLICY enforcement — owned by `t35-server-policy-access-control.md`; the skill
  documents how an agent *requests* least privilege, not the enforcement.
- Credential acquisition flows — owned by `t27`/`t19`; the skill assumes creds resolved.

## Key components

- `crates/cfs-skill/` — assets crate carrying the authored `SKILL.md` and the golden corpus
  under `assets/examples/`. Embedded via `include_str!`/`include_dir!` so the loop docs and
  examples ship inside the single binary (RFD §9).
- `cfs-cli` `describe` subcommand: `fn cmd_describe(path: &Path, json: bool) -> Result<Output>`
  that calls the engine's existing `Driver::describe` and renders via the t29 output layer.
- `DescribeReport` DTO (in `cfs-core::describe`):
  ```rust
  pub struct DescribeReport {
      pub path: String,
      pub archetype: Archetype,            // Blob | Relational | Append | ObjectGraph
      pub columns: Vec<ColumnInfo>,        // name + Type, from source catalog (owned DTO)
      pub verbs: Capabilities,             // which universal verbs this node supports
      pub procedures: Vec<ProcSig>,        // CALL driver.action(..) signatures
      pub aliases: Vec<AliasSig>,          // prelude pure fns, e.g. SEND -> Plan
      pub pushdown: PushdownSummary,
  }
  ```
  These are **owned DTOs** — no vendor SDK types leak (RFD §9). `serde::Serialize` only;
  the JSON is the agent-facing contract.
- `cfs-skill::golden` — a test-only harness loading each example, parsing + evaluating to a
  `Plan`, and asserting the PREVIEW rendering against a checked-in golden (plan assertion,
  no COMMIT, no network).
- No new keywords, paths, functions, or codecs are introduced — this ticket strictly
  consumes the closed core and the three open registries (RFD §3).

## Implementation steps

1. Define `DescribeReport` + sub-DTOs in `cfs-core::describe`; derive `Serialize`. Map each
   `Archetype` (RFD §5) to its native verb hint string for the agent.
2. Wire `cfs describe <path> [-json]` in `cfs-cli` over the t13 `Driver::describe` hook,
   rendering through the t29 output layer (human table / JSON).
3. Create `crates/cfs-skill/` with `assets/SKILL.md`; embed at compile time.
4. Author `SKILL.md`: the four-step loop; one canonical example per archetype; an explicit
   "respect the closed core, extend only via paths/functions/codecs" rule; an "always PREVIEW
   before COMMIT; treat `irreversible` plan nodes as gates" rule (RFD §6, §10).
5. Author the golden corpus — for each: a one-line intent, the `DESCRIBE` excerpt, the cfs
   statement, the expected PREVIEW plan:
   - mail: `INSERT INTO /mail/drafts …` then `… |> CALL mail.send` (and `SEND` alias).
   - drive: `cp /local/report.pdf /drive/Reports/` (blob archetype).
   - github: `CALL github.merge(method=>'squash')` on a PR object-graph node.
   - slack: `INSERT INTO /slack/#chan/messages VALUES …` (append archetype).
   - sql: `FROM /sql/pg/orders |> WHERE total > 100 |> SELECT id,total` (pushdown, pure).
   - git: `INSERT INTO /git/repo/commits …` and read `/git/repo@<ref>/path`.
   - server: `CREATE TRIGGER … ON … DO <plan>` desugaring to `INSERT INTO /server/triggers`.
6. Add the `cfs-skill::golden` test harness; mark examples no-live-creds (mock/in-memory driver).
7. Cross-link `SKILL.md` from the repo docs index and the RFD §11 E8 row.

## Considerations

- **Hard part — keeping the loop genuinely uniform.** The temptation is per-driver special
  cases in the skill. Resolve by making `DESCRIBE` output the *only* thing the agent reads:
  if the loop needs prose exceptions, the driver contract (t13) is under-declaring and should
  be fixed there, not papered over in the skill. The golden corpus enforces this — every
  example uses the identical four steps.
- **Least privilege & secrets (RFD §10).** The skill instructs the agent to scope plans to the
  minimum drivers/verbs and, on the server, to request a `POLICY` (t35); it must never echo or
  log resolved credentials. `DescribeReport` carries schema/capabilities only — never secret
  material.
- **Idempotency / recovery (RFD §6).** Document `UPSERT` as the retry-safe default and
  `@version`/ETag optimistic concurrency for read-then-write; document that `cp` is
  copy→verify→delete and that the audit ledger is the recovery source of truth.
- **Observability.** Examples show reading the PREVIEW plan's affected counts and the
  `irreversible` flag; the skill frames PREVIEW-as-CI-test (RFD §8) for unattended handlers.
- **Directory/coding standards.** Skill assets live under `crates/cfs-skill/assets/`; DTOs in
  `cfs-core`; no vendor types in the JSON contract; goldens are deterministic (no clocks/UUIDs
  unless stubbed).
- **Plan-assertion bias.** Because COMMIT does I/O, all acceptance tests assert the *plan*, not
  side effects — matching the purity invariant (RFD §3): every fn is `… -> Plan`.

## Acceptance criteria

- `cargo build` and `cargo clippy --all-targets -- -D warnings` are green.
- `cfs describe /mail/drafts -json` emits a `DescribeReport` JSON with `archetype`, `columns`,
  `verbs`, `procedures`, `aliases` populated; human form renders a readable table.
- `SKILL.md` documents the four-step loop and contains ≥1 worked example for each of
  mail, drive, github, slack, sql, git, and a `/server/...` binding, each using the identical
  DESCRIBE → statement → PREVIEW → COMMIT structure.
- Golden tests: every example parses, evaluates to a `Plan`, and its PREVIEW rendering matches
  the checked-in golden; **no COMMIT and no network** are performed (no-live-creds).
- A negative golden asserts that a statement using an unsupported verb for a node fails at
  parse/resolve time with a structured error (RFD §5) — the agent-legible failure path.
- The skill is discoverable from the docs index; RFD §11 E8 references it.
