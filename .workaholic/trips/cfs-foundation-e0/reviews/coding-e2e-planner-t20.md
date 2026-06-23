# Coding E2E — Planner — t20 (Gmail driver)

- Author: Planner (Progressive)
- Phase: Coding / review-and-testing
- Target: t20 — Driver: Gmail (`cfs-driver-gmail`)
- Method: external-consumer E2E against a **mocked** GmailClient (scripted responses + recorded
  requests). **No live Gmail, no network, no credentials.** The token-canary item additionally
  drives the **real** `GoogleApiGmailClient` over a recording `HttpExchange`.
- Harness: throwaway crate at `/tmp/cfs_t20_e2e` (own `[workspace]`, path-deps on
  `driver-gmail`, `runtime`, `driver`, `plan`, `types`, `codec`, `secrets`, `google-auth`,
  `http-core`; no production code). 10 tests + 1 evidence print. Removed after the run.
- Result: **10 passed / 0 failed**; evidence print green.

## Verdict: E2E approved (with one minor seam-ergonomics observation)

No token leak. No permanent-delete path. REMOVE trashes. Send is irreversible and PREVIEW does
no I/O. All six required items PASS.

---

## PASS/FAIL per item

### Item 1 — List/search → rows (q= pushdown + local residual) — PASS
A query over `/mail/INBOX` with `from = 'alice@example.com' AND subject LIKE 'weekly' AND
(from='a@x' OR from='b@x')`:
- The mapped conjuncts pushed to the Gmail `q=`; the unmapped `OR` stayed **residual** (filtered
  locally — over-fetch then filter, result correct).
- Recorded `q=` (exact): `label:INBOX from:alice@example.com subject:weekly`
- Recorded residual (kept local): `Or(Cmp(from, Eq, "a@x"), Cmp(from, Eq, "b@x"))`
- The search returns ids only; the per-id detail fetch is a **separate** `GetMessage` op (the
  N+1 leaf the planner fans out), not an inline loop — confirmed in the recorded call sequence
  (`Search{..}` then `GetMessage{id:"m1"}`) with `max_results: Some(5)` riding along.
- Decode-to-typed-row: the search→id→detail→`MailMessage::to_row()` typed decode is independently
  covered by the crate's in-crate unit test `search_pushes_q_and_decodes_message_rows`
  (asserts `Value::Text("m1")` / `Value::Text("alice@example.com")`). See the observation below
  re. why my external harness cannot re-seed that fixture row.

### Item 2 — Capabilities/safety (writes rejected; REMOVE → TRASH) — PASS
- A message node (`id:m1`) rejects `INSERT`/`UPSERT`/`UPDATE` at the parse-time capability gate
  (`check_capability(...).code() == "unsupported_verb"`); only `SELECT`/`REMOVE` allowed.
- `/mail/drafts` rejects `UPDATE`.
- **REMOVE → TRASH, never permanent delete.** Recorded API call for `REMOVE id:m1`:
  `[TrashMessage { id: "m1" }]`. For `REMOVE id:thread:t1`: `[TrashThread { id: "t1" }]`.
- **Hard proof of trash-not-delete:** the `GmailClient` trait surface has **no permanent-delete
  method at all** (`messages.delete` is intentionally unimplemented per RFD §10) — a hard delete
  is structurally unreachable, and the recorded op is specifically the `Trash*` variant.

### Item 3 — Send (well-formed MIME raw; irreversible) — PASS
`CALL mail.send` with draft content → create-then-send (recoverable de-dupe path). Recorded:
`CreateDraft{raw}` then `SendDraft{..}` in order.
- The `raw` is the Gmail base64url field (no `+` / `/` — confirmed `raw-has-plus-or-slash:
  false`).
- Decoded MIME (`café ☕ report` subject → RFC 2047; `hello\nworld` body → CRLF):
  ```
  To: bob@example.com\r\n
  Subject: =?UTF-8?B?Y2Fmw6kg4piVIHJlcG9ydA==?=\r\n
  MIME-Version: 1.0\r\n
  Content-Type: text/plain; charset="UTF-8"\r\n
  \r\n
  hello\r\nworld
  ```
  CRLF line endings, RFC 2047 base64 non-ASCII subject, correct `Content-Type` — well-formed.
- **Irreversible:** the send node carries `irreversible: true`; `procedures()` declares
  `mail.send` with `irreversible == true` and `requires_scopes == [gmail.compose]`; the `SEND`
  prelude alias desugars to `mail.send`.
- **PREVIEW warns + no auto-retry:** `preview(plan).rows[0].irreversible == true`, and the mock
  recorded **zero** API calls during PREVIEW (no I/O, no auto-retry).

### Item 4 — Multi-account isolation — PASS
Two accounts → two independent driver instances over two independent mock clients. A label-modify
routed to account A's driver recorded **only** on A's client; B's client stayed empty. A
subsequent trash on B recorded only on B; A unchanged. An op on A never touches B's
client/token. (Account selection is the t19 base: one `GoogleApiClient`/`GmailClient` per account
bound at construction; the driver is account-agnostic, so cross-account bleed is structurally
impossible.)

### Item 5 — Token safety (canary bearer absent everywhere) — PASS
Drove the **real** `GoogleApiGmailClient` over a recording `HttpExchange`, with a planted canary
access token `PLANTED-LEAK-CANARY-google-3a2b1c0d-ya29-TOKEN` minted by a scripted OAuth refresh.
Captured tracing at `TRACE` into a buffer. Four proofs:
- **Proof A (sanity):** the canary IS the real bearer on the wire (`Authorization: Bearer
  <canary>` present in the recorded request) — so the call genuinely carried it.
- **Proof B:** the request's `Debug` redacts the canary — `format!("{req:?}")` does **not**
  contain the canary (the `SENSITIVE_HEADERS` redaction boundary holds).
- **Proof C:** the captured TRACE-level tracing output contains **no** canary.
- **Proof D:** error surfaces are secret-free — a forced 401 (refresh + retry both 401) yields a
  `GmailError` whose `Display` + `Debug` contain neither the canary nor the literal `Bearer`.

### Item 6 — End-to-end COMMIT through interpreter + bridge — PASS
- `mail.send` plan committed through `Interpreter::with_defaults` + `PlanApplierBridge`:
  `outcome.is_complete()`, `applied_ids == [NodeId(0)]`; recorded calls `CreateDraft` then
  `SendDraft` (never a permanent delete); **ledger** records 1 leg, `irreversible == true`,
  `LegStatus::Applied`.
- `REMOVE id:m1` committed end-to-end: recorded `[TrashMessage { id: "m1" }]`; ledger leg
  `irreversible == true`.
- **No panics on adversarial inputs:** undeclared `CALL mail.nuke`, draft `INSERT` with no
  recipients, a write to a non-`/mail` path (`/etc/passwd`), `REMOVE` of a label collection, and
  an empty `id:` selector all return structured errors (no panic).

---

## Concern + proposal (Critical Review Policy)

**Concern (minor, business/consumer ergonomics — NOT a blocker):** the public DTO `MailMessage`
is `#[non_exhaustive]` with **no public constructor** and no `Default`. The driver's own in-crate
tests build it via `super::*` (where `non_exhaustive` does not apply), but a **downstream
consumer** of the crate cannot fabricate a `MailMessage` to seed `MockGmailClient::with_message`
in their own integration tests. This left my external harness unable to seed a fixture message
for the typed-row decode assertion in Item 1 (I relied on the crate's internal unit test for that
specific leg). Same applies to `MailDraft`/`Attachment` (`#[non_exhaustive]`, no builder) for any
downstream consumer wanting to assemble a draft DTO directly. This does not affect the driver's
runtime behavior — drafts are assembled internally from effect row columns — so it is an
ergonomics gap for external test authors, not a correctness defect.

**Proposal (business outcome — make the mock seam usable by downstream consumers):** add a
`#[cfg(any(test, feature = "test-util"))]` constructor or builder for `MailMessage` (and
`MailDraft`), e.g. `MailMessage::for_test(id, thread_id, from, subject)` or a small
`MailMessageBuilder`, exported behind a `test-util` feature. This lets any consumer seed the
already-public `MockGmailClient` in their own tests and keeps the no-vendor-leak / non_exhaustive
guarantees intact (the door is test-only). The value: the mockable seam the crate advertises as a
flagship capability ("mocks in tests so no live network is touched") becomes usable by the
consumers it is meant for, not only by the crate's own tests.

---

## Blocking-condition checks (explicit)

- Token leak → **NONE** (Item 5, four proofs). Not blocked.
- REMOVE that permanently deletes → **NONE**; REMOVE maps to TRASH and the client surface has no
  delete method at all (Item 2). Not blocked.

**Overall: E2E approved.**
