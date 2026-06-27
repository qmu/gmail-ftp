# qfs roadmap — ticket index (M0 → M+)

Design anchors: **`docs/roadmap.md`** (the vision + phased delivery plan) and **RFD 0001**
(`.workaholic/RFDs/0001-qfs-architecture.md`, the from-scratch architecture). Every ticket below
references both.

> **Status: 40 tickets drafted (todo) — `t42…t81`.** They live in
> [`.workaholic/tickets/todo/a-qmu-jp/`](.workaholic/tickets/todo/a-qmu-jp/) as `2026062*-t42…t81-*.md`.
> This is the build-out of the roadmap on top of the delivered E0–E8 foundation (the 41 `t01–t41`
> tickets in [`.workaholic/tickets/archive/work-20260622-230954/`](.workaholic/tickets/archive/work-20260622-230954/), all shipped). The current
> binary is **0.0.8**; per CLAUDE.md every shipped ticket is its own PR + patch bump + `v0.0.x` tag.

## How to read this

Each ticket carries the standard frontmatter and five sections (Overview / Exact seams /
Implementation steps / Key files / Considerations), is grounded in real crate/file/symbol seams,
and obeys the qfs safety floor (**describe pure · preview touches nothing · commit explicit ·
irreversible needs an extra ack**), the dep-direction guard (`crates/cmd/tests/dep_direction.rs`),
and the closed-core language governance (`crates/lang/src/keywords.rs` freeze tests).

`depends_on` is recorded as full ticket filenames (the `validate-ticket.sh` hook requires it). The
build order below follows the dependency graph; **M0 is the foundation everything else needs.**

## Recommended build order

`M0 → M1 → M2 → {M3, M6} → M4 → M5 → {M7, M8} → M9 → M+`
(M6 language work, M8 external scheduling, and M+ `fs` driver are independent and can be slotted in
parallel; M3 needs only M2.)

---

## M0 — Persistence foundation
*The single world the dashboard, CLI, and MCP agree on. "Architecture first" (decision A); all
SQLite + envelope encryption (E); scrap the file vault (E); `accounts`→`connections` (B).*

- **t42** — SQLite System DB + Project DB + embedded migrations runner · depends_on: _(none)_
- **t43** — Envelope-encrypted credential store on SQLite (scrap the file vault) · depends_on: t42
- **t44** — Rename `accounts` → `connections` (free `accounts` for human identity) · depends_on: t43
- **t76** — Hash-chained audit event emission (`/sys/audit` live view; emit-don't-store, decision V) · depends_on: t42

## M1 — Identity store
*A real "who" at every tier (decisions B; §4.1 identity ≠ authorization).*

- **t45** — `users` + `accounts` identity tables + local sign-up · depends_on: t42
- **t46** — Session handling · depends_on: t45

## M2 — Server-as-MCP + OAuth AS
*The agent's single endpoint (decisions C, K). NEW crates — no MCP/OAuth-AS code exists yet; the
OAuth **client** (`crates/google-auth`) and the HTTP listener (`crates/http`) already do.*

- **t47** — MCP server: describe/preview/commit/connections tools · depends_on: t42
- **t48** — OAuth 2.1 AS: PRM (RFC 9728) + AS metadata (RFC 8414) + JWKS · depends_on: t45
- **t49** — Dynamic client registration (RFC 7591) + auth-code + PKCE · depends_on: t48, t46
- **t50** — Bearer + refresh tokens guarding the MCP endpoint · depends_on: t49, t47

## M3 — Dashboard at parity
*The second face; admin page begins. One engine, three faces.*

- **t51** — Embedded SPA dashboard over the same engine · depends_on: t47
- **t52** — preview→commit approval cards · depends_on: t51, t50
- **t53** — `/sys/*` driver + first admin views · depends_on: t42, t45
- **t77** — Externalized telemetry: `file`/`stdout`/`OTel` sinks for audit/metrics/traces (decision V) · depends_on: t53, t76
- **t78** — Audit-chain sealing to an external WORM/transparency log (decision V) · depends_on: t76, t53

## M4 — Cloud tier
*Local + Cloud usage. Clients already exist — this is consent UX + wiring + live verification.*

- **t54** — Cloud `connections` consent flows; sign-in mandatory for cloud drivers · depends_on: t44, t49

## M5 — Self-hosted multi-user
*Teams on their own server (decisions D, I, J).*

- **t55** — Invites (email / one-time URL) + membership · depends_on: t45
- **t56** — Upstream OIDC federation (hub model) · depends_on: t48, t45
- **t57** — Extended `POLICY` / ACL language · depends_on: t53
- **t58** — `/directory/...` driver (LDAP/AD/Entra/Workspace) · depends_on: t57
- **t59** — Selectable AI safety modes (3 presets) · depends_on: t52, t50
- **t79** — Credential rotation & revocation (re-mint + DEK re-wrap on offboarding, decision U) · depends_on: t43, t44, t57, t55
- **t80** — Per-recipient (E2E) DEK wrap for high-sensitivity connections (decision U) · depends_on: t43, t45, t59
- **t81** — Self-hosted team-shared connections (project-owned, actor-policy gated, decision U/§3.3) · depends_on: t43, t44, t57, t71

## M6 — Language core
*Part 1.2/1.3 expressiveness + grammar shape (decisions G, H, O, P, Q, R, S, T). `let` and
`transaction` are GENUINE new keywords (deliberate freeze-test edits); lambdas + map/filter/reduce are
"functions are values" — no `DEF` keyword; t73 **removes** `from`; t74 lowercases the keyword set;
t70/t72 change operator/grammar shape; t71 is resolution; t75 adds the static type system.*

- **t60** — `let` binding (also brings `;`-free, multi-statement parsing) · depends_on: _(none)_
- **t61** — Lambdas as values + `map`/`filter`/`reduce` (named fns are `let`-bound lambdas — no `DEF`) · depends_on: t60
- **t62** — Reversible-only `transaction` + commit-point ordering · depends_on: _(none)_
- **t70** — Operator split: `=` always binds, `==` compares (decision O) · depends_on: _(none; ship with/before t60)_
- **t71** — Path expression: scope realms + reserved-name resolution (decision P) · depends_on: t44 _(foundational — M4/M5/M7 assume it)_
- **t72** — Write-form grammar: writes as pipeline stages (decision Q) · depends_on: _(none; coordinate with t70)_
- **t73** — `Resource` literal: drop `from`, unquote `policy`/`member_of` paths (decision R) · depends_on: t70
- **t74** — Lowercase the closed keyword set (decision S) · depends_on: t70, t73
- **t75** — Static primitive type system, checked at plan time (decision T) · depends_on: t61, t73

## M7 — Agent fabric *(qfs Cloud)*
*Part 3.3 fleet (decisions L, N). Tunnel requires a qfs Cloud sign-in.*

- **t63** — qfs-native outbound tunnel + relay · depends_on: t50
- **t64** — `/claude/...` driver (AI sessions) · depends_on: t63, t47

## M8 — External scheduling
*Part 4.3 (decision M, revised). qfs is not a scheduler: OS cron (individual) + Cloudflare Cron
Triggers (managed) drive the invokable unit; no internal scheduler.*

- **t65** — Externalize scheduling (OS cron + Cloudflare Cron Triggers); retire the internal scheduler · depends_on: _(none)_

## M9 — Managed Team *(qfs Cloud)*
*The top tier.*

- **t66** — qfs Cloud OAuth brokering + team connections · depends_on: t54, t56
- **t67** — Billing (free individual / paid team) · depends_on: t66

## M+ — Expansions
*Candidates the foundation makes cheap — not commitments (Part 5).*

- **t68** — First-class `fs` driver (real filesystem blob namespace) · depends_on: _(none)_
- **t69** — Expansions umbrella (CDC · driver SDK/registry · credential brokering · approvals ·
  richer telemetry analytics · agent mesh) · depends_on: t64, t57
  *(observability itself is now confirmed — decision V, tickets t76–t78 — not an M+ candidate)*

---

## Notes for whoever picks these up

- **Foundation gate.** t42 unblocks the most; nothing in M1–M9 should start before the
  persistence/migration seam exists. t60/t62 (language) and t68 (`fs` driver) are the only items
  with no M0 dependency.
- **Honesty first.** Each slice updates docs/skill/README to match exactly what now works; never
  document a capability before it ships (the rule that the existing
  `…-wire-real-execution-and-auth.md` ticket exists to enforce).
- **Generated docs.** `docs/{language,drivers,server}.md` are rendered by
  `cargo run -p xtask -- gen-docs` — never hand-edit; the anti-drift `--check` must stay green.
- **Open product decisions are flagged, not guessed** inside the tickets: DB-on-disk location and
  passphrase-source UX (t42/t43), keyword case policy (case-insensitive vs strictly-lowercase, t74),
  payment provider (t67), and the admin-page shape the roadmap itself leaves open (t53).
- **Layer vocabulary.** The `validate-ticket.sh` hook accepts only `UX, Domain, Infrastructure, DB,
  Config`; the roadmap's "Application/Interface" framing was mapped onto these (mostly
  Infrastructure/UX/Domain). A later cleanup pass could normalise the two tickets that map
  `Application` differently — cosmetic only.
