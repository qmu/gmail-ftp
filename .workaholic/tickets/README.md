# cfs rebuild — ticket index

Design anchor: **RFD 0001** (`.workaholic/RFDs/0001-cfs-architecture.md`) is the single source of truth for every ticket below.

Recommended build order follows the dependency graph: **E0 → E1 / E2 / E3 foundations → E4 drivers → E5 / E6 → E7 server → E8 cross-cutting.**

## E0 — Foundations

- [ ] **t01** — Rust workspace & single-binary scaffold · depends_on: _(none)_
- [ ] **t02** — Parser library decision spike · depends_on: t01

## E1 — Language core

- [ ] **t03** — Lexer / tokenizer · depends_on: t02
- [ ] **t04** — Grammar, AST & governance · depends_on: t03
- [ ] **t05** — Type & schema model · depends_on: t04
- [ ] **t06** — Name resolution, CALL & aliases · depends_on: t04, t05
- [ ] **t07** — Evaluator to effect-plan · depends_on: t04, t05, t09
- [ ] **t08** — Stdlib & driver preludes · depends_on: t06

## E2 — Effect-plan & runtime

- [ ] **t09** — Effect-plan & PREVIEW/COMMIT · depends_on: t05
- [ ] **t10** — Interpreter: batch & parallel · depends_on: t09, t13
- [ ] **t11** — Transactions, idempotency & concurrency · depends_on: t10
- [ ] **t12** — Audit ledger & observability · depends_on: t10

## E3 — Federation & data

- [ ] **t13** — Driver contract trait · depends_on: t05, t09
- [ ] **t14** — Pushdown planner & local engine · depends_on: t13, t10
- [ ] **t15** — Codec registry (DECODE/ENCODE) · depends_on: t05

## E4 — Drivers

- [ ] **t16** — Driver: local filesystem · depends_on: t13, t15
- [ ] **t17** — Driver: SQL databases · depends_on: t13, t14
- [ ] **t18** — Driver: HTTP / REST generic · depends_on: t13, t15
- [ ] **t19** — Driver: Google OAuth multi-account · depends_on: t13, t27
- [ ] **t20** — Driver: Gmail · depends_on: t19
- [ ] **t21** — Driver: Google Drive · depends_on: t19
- [ ] **t22** — Driver: object storage (S3 / R2) · depends_on: t13
- [ ] **t23** — Driver: Cloudflare D1 / KV / Queues · depends_on: t13, t17
- [ ] **t24** — Driver: GitHub · depends_on: t13, t18
- [ ] **t25** — Driver: Slack · depends_on: t13, t18
- [ ] **t26** — Driver: git object model · depends_on: t13, t15

## E5 — Auth / credentials

- [ ] **t27** — Credential secret store & resolution · depends_on: t01

## E6 — CLI

- [ ] **t28** — CLI: interactive shell · depends_on: t09, t16
- [ ] **t29** — CLI: one-shot & output · depends_on: t09

## E7 — Server

- [ ] **t30** — Server runtime & self-config driver · depends_on: t09, t13
- [ ] **t31** — Server binding DDL · depends_on: t30
- [ ] **t32** — Server: HTTP endpoints · depends_on: t31
- [ ] **t33** — Server: scheduler / jobs · depends_on: t31
- [ ] **t34** — Server: event bus, webhooks & watchers · depends_on: t31
- [ ] **t35** — Server: policy & access control · depends_on: t31, t13

## E8 — Cross-cutting (security, test, docs, AI procedure)

- [ ] **t36** — Deployment targets · depends_on: t30
- [ ] **t37** — Security threat model · depends_on: t27, t35
- [ ] **t38** — Test harness & golden tests · depends_on: t13
- [ ] **t39** — AI operating procedure & skill · depends_on: t29, t30
- [ ] **t40** — Docs & distribution · depends_on: t36
