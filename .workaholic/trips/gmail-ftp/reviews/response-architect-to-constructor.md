# Response — Architect to Constructor (Round 1)

Author: Architect
Re: `reviews/round-1-constructor.md` — "Request revision" on `models/model-v1.md`
Decision: **Accept and revise** (new artifact: `models/model-v2.md`)

---

I accept the request-revision. Both must-fix concerns (C1, C2) are correct and
I have addressed them in `model-v2.md`; the two minor concerns (C3, C4) are also
folded in. Below I respond to each.

## On the central navigation decision (C2 — "close it, don't punt")

You are right that a model which defers its own central modeling decision is not
yet a buildable bridge. v1 *recommended* the message-as-default-leaf (§3 item 3)
but then re-listed "default leaf granularity" as an open question in §5 — that is
the contradiction you flagged, and it is mine to fix.

The team has converged on **2-level navigation** (`root → label → message`,
attachments as leaves inside a message, thread access opt-in via `threadId` and
`id:thread:<id>`). This matches my own round-1 recommendation and your
implementation-complexity / testability rationale (one N+1 tier not two, smaller
`Ref.Kind` set, crisper `get` target). In v2 this is no longer a recommendation
or an open question: it is the **canonical** mapping. Concretely:

- §2 commits to the 2-level mapping as decided, not proposed.
- The cwd-stack `Ref.Kind` is **narrowed to `{label, message}`** — exactly the
  two-kinds resolver you wanted, removing the `thread` kind from the path-depth
  question entirely. The thread is no longer a navigable depth; it is metadata.
- §5's "default leaf granularity" open question is **deleted** and replaced with
  a recorded resolution. The scope question is likewise closed
  (`gmail.modify` + `gmail.compose`, never full `https://mail.google.com/`),
  matching your Design §2/R1 and the Planner's least-privilege ruling.

The thread becomes a strictly one-layer-down concern living inside
`internal/gmail`: surfaced as a `threadId` field on every DTO row and reachable
via `id:thread:<id>` addressing plus an opt-in `get id:thread:<id>` → `.mbox`
export. No `cd thread`, no second N+1 tier, no resolver disambiguation at a
shared depth.

## On the N+1 cost model (C1 — the dominant cost driver, under-modeled)

Accepted in full. Your verified fact is the load-bearing one and v1 buried it as
a metaphor-clarity nuance in §3 item 3. The Gmail `list` endpoints
(`messages.list`, `threads.list`) return **IDs only** — `messages.list` yields
`{id, threadId}`; `threads.list` yields `{id, historyId, snippet}` with no
subject and explicitly no message list. Therefore any human-readable listing is
an **unavoidable N+1**: one `list` call plus one `get(format=metadata)` per row.
This is the product's dominant runtime cost, not a footnote.

v2 adds an explicit **runtime cost model** (new §3a) that commits to:

- **Batched metadata fetch.** Per-row
  `messages.get(format=metadata, metadataHeaders=Subject,From,Date)` issued
  through the batch/HTTP-pipeline path, not serial round-trips — only the headers
  the listing renders, never full bodies, during `ls`.
- **Pagination with a capped default page size.** A bounded first page
  (`maxResults`) plus `nextPageToken` continuation, so the first `ls` on a large
  label is a fast partial list with a "showing N of many" hint rather than a
  stall — answering the Planner's responsiveness concern too.
- **Where caching and quotas bite.** An intra-command message-metadata cache
  keyed by message ID (so `cd`-then-`ls`-then-`get` does not re-fetch); the
  explicit note that per-row `get`s, not the `list`, are what consume Gmail
  per-user quota units and drive latency; and that the 2-level mapping is what
  keeps this to **one** N+1 tier (a 3-level thread tier would add a second).

This folds the cost model *into* the navigation decision exactly as you proposed:
2-level is chosen partly *because* it minimizes the number of N+1 tiers.

## On the minor concerns

- **C3 (label/send verbs beyond the gdrive-ftp set).** v2 records the resolution:
  v1 ships **read + trash + draft**; the one irreversible action (`send`) is a
  separate, explicitly-verbed, audited, recipient-echoing action isolated to a
  v1.1 increment, and message-level `label add/rm` is deferred unless its
  audit/echo UX ships with it. The irreversible-`send`-as-separate-verb decision
  (v1 §3 item 5) is preserved unchanged — `put` never sends.
- **C4 (table convergence).** With 2-level made canonical, my mapping table is the
  agreed one; I note in v2 that `design-v2.md` aligns to it, so the two artifacts
  ship one hierarchy, not two.

## Translation-fidelity note

v2 adds a fidelity note making explicit that "label = view/filter, not owner"
(a message legitimately appears under multiple labels) and that the N+1 is an
inherent property of the Gmail data shape, not an implementation defect — so the
metaphor's cost is documented in README/SKILL rather than discovered at runtime.

Boundary integrity (auth / backend-client / shell / audit mirroring gdrive-ftp,
owned DTOs never marshaling the vendor struct) is preserved unchanged in v2.

Net: both must-fix concerns closed, both minor concerns folded in, navigation
made canonical, cost model made first-class. Proceeding to `model-v2.md`.
