# qfs documentation index

The `qfs` workspace (the Rust rebuild of this repo, RFD-0001) documents itself in three places:

## Architecture & rationale
- **RFD-0001 — qfs architecture**: [`.workaholic/RFDs/0001-qfs-architecture.md`](../.workaholic/RFDs/0001-qfs-architecture.md)
  — the closed core, the three open registries, the effect-plan / PREVIEW→COMMIT model, the driver
  contract, federation/pushdown, and the ticket epics (§11).
- **ADRs** ([`docs/adr/`](adr/)) — the recorded technical decisions (parser library, local combine
  engine, git object access, HTTP serving, deployment hosts, test harness).
- **Security** ([`docs/security/`](security/)) — the threat model.

## AI operating procedure (the payoff — RFD §1, epic E8)
`qfs` exists **for AI**: an agent learns *one* small grammar and *one* loop instead of N SDKs. The
loop is **DESCRIBE `<path>` → write a qfs statement → PREVIEW → COMMIT**.

- **The agent skill**: [`crates/skill/assets/SKILL.md`](../crates/skill/assets/SKILL.md) — the
  authored operating procedure: the four-step loop, the four archetypes, the rules (respect the
  closed core; always PREVIEW before COMMIT; least privilege; idempotency/recovery), and one worked
  example per driver (mail, drive, github, slack, sql, git, and a `/server/...` binding), each using
  the identical four steps. Embedded in the single `qfs` binary via `include_str!` so it ships with
  the tool.
- **Discover a node's contract**: `qfs describe <path> [-json]` emits the stable `DescribeReport`
  (archetype, columns, supported verbs, `CALL` procedures, prelude aliases, pushdown) the agent
  reads as step 1. `DESCRIBE` is pure — no creds, no I/O, no network.
- **The golden corpus**: [`crates/skill/tests/golden_corpus.rs`](../crates/skill/tests/golden_corpus.rs)
  proves every skill example parses → evaluates → its PREVIEW matches a checked-in golden, with no
  COMMIT and no live credentials, plus a negative golden (an unsupported verb fails with a
  structured error). The uniformity of those four steps across drivers IS the deliverable.
