# cfs rebuild — ticket index

Design anchor: **RFD 0001** (`.workaholic/RFDs/0001-cfs-architecture.md`) is the single source of truth for every ticket below.

> **Status: all 41 tickets delivered** (trip `cfs-foundation-e0`, branch `work-20260622-230954`, 1240 tests green). Archived under `.workaholic/tickets/archive/work-20260622-230954/`; each ticket's `commit_hash` records its acceptance commit. Ready for `/report` + `/ship`.

Recommended build order follows the dependency graph: **E0 → E1 / E2 / E3 foundations → E4 drivers → E5 / E6 → E7 server → E8 cross-cutting.**

## E0 — Foundations

- [x] **t01** — Rust workspace & single-binary scaffold · depends_on: _(none)_
- [x] **t02** — Parser library decision spike · depends_on: t01

## E1 — Language core

- [x] **t03** — Lexer / tokenizer · depends_on: t02
- [x] **t04** — Grammar, AST & governance · depends_on: t03
- [x] **t05** — Type & schema model · depends_on: t04
- [x] **t06** — Name resolution, CALL & aliases · depends_on: t04, t05
- [x] **t07** — Evaluator to effect-plan · depends_on: t04, t05, t09
- [x] **t08** — Stdlib & driver preludes · depends_on: t06

## E2 — Effect-plan & runtime

- [x] **t09** — Effect-plan & PREVIEW/COMMIT · depends_on: t05
- [x] **t10** — Interpreter: batch & parallel · depends_on: t09, t13
- [x] **t11** — Transactions, idempotency & concurrency · depends_on: t10
- [x] **t12** — Audit ledger & observability · depends_on: t10

## E3 — Federation & data

- [x] **t13** — Driver contract trait · depends_on: t05, t09
- [x] **t14** — Pushdown planner & local engine · depends_on: t13, t10
- [x] **t15** — Codec registry (DECODE/ENCODE) · depends_on: t05

## E4 — Drivers

- [x] **t16** — Driver: local filesystem · depends_on: t13, t15
- [x] **t17** — Driver: SQL databases · depends_on: t13, t14
- [x] **t18** — Driver: HTTP / REST generic · depends_on: t13, t15
- [x] **t19** — Driver: Google OAuth multi-account · depends_on: t13, t27
- [x] **t20** — Driver: Gmail · depends_on: t19
- [x] **t21** — Driver: Google Drive · depends_on: t19
- [x] **t22** — Driver: object storage (S3 / R2) · depends_on: t13
- [x] **t23** — Driver: Cloudflare D1 / KV / Queues · depends_on: t13, t17
- [x] **t24** — Driver: GitHub · depends_on: t13, t18
- [x] **t25** — Driver: Slack · depends_on: t13, t18
- [x] **t26** — Driver: git object model · depends_on: t13, t15
- [x] **t41** — Driver: Google Analytics (GA4, read-only) · depends_on: t13, t19

## E5 — Auth / credentials

- [x] **t27** — Credential secret store & resolution · depends_on: t01

## E6 — CLI

- [x] **t28** — CLI: interactive shell · depends_on: t09, t16
- [x] **t29** — CLI: one-shot & output · depends_on: t09

## E7 — Server

- [x] **t30** — Server runtime & self-config driver · depends_on: t09, t13
- [x] **t31** — Server binding DDL · depends_on: t30
- [x] **t32** — Server: HTTP endpoints · depends_on: t31
- [x] **t33** — Server: scheduler / jobs · depends_on: t31
- [x] **t34** — Server: event bus, webhooks & watchers · depends_on: t31
- [x] **t35** — Server: policy & access control · depends_on: t31, t13

## E8 — Cross-cutting (security, test, docs, AI procedure)

- [x] **t36** — Deployment targets · depends_on: t30
- [x] **t37** — Security threat model · depends_on: t27, t35
- [x] **t38** — Test harness & golden tests · depends_on: t13
- [x] **t39** — AI operating procedure & skill · depends_on: t29, t30
- [x] **t40** — Docs & distribution · depends_on: t36
