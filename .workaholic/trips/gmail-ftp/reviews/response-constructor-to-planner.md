# Response — Constructor to Planner (Round 1)

Author: Constructor
Re: `reviews/round-1-planner.md` — Request revision on `designs/design-v1.md`
Decision: **Accept and revise.** New artifact: `designs/design-v2.md`.

---

I accept the revision request in full. The Planner's three concerns are well
founded, and the navigation-default divergence is exactly the open question I
flagged in my own §3 quality notes. The team has converged on the **2-level
navigation** model (root → label → message, attachments as leaves inside a
message, threads opt-in via a `threadId` field and `id:thread:<id>` addressing).
This is the same default I recommended after verifying that Gmail's `list`
endpoints return IDs only — a thread tier costs an N+1 metadata fetch per row for
no navigational gain — so adopting it resolves both the divergence and the
latency tax. Each concern is addressed below; all changes land in **design-v2**.

## Concern (a) — 3-level vs Model divergence

**Accepted.** design-v1's §0 mapping table introduced a `label → thread →
message` tier that the Model (§2/§3.3) deliberately collapsed to two conceptual
levels. design-v2 adopts the canonical **2-level** model: root lists labels, a
label lists messages, and a message's attachments are leaves inside it. The
contested third tier is removed. Concretely, `Ref.Kind` is narrowed from
`{label, thread, message}` to **`{label, message}`** — you never `cd` into a
thread. `threadId` becomes a *field* on the message entry/DTO, and `GetThread`
batching survives only as an implementation detail inside `internal/gmail`,
surfaced via `id:thread:<id>` and an opt-in `.mbox`/thread export for power
users. `pwd` now prints `/INBOX` (label) with the message as the leaf, never
`/INBOX/<thread-subject>`. The resolved default is written verbatim into the
README command table and SKILL.md so a persona's first `ls` matches the docs.

## Concern (b) — `rm` blast radius

**Accepted.** design-v1's `rm` row ("trash a thread (default) or message") could
silently trash a whole conversation when the user meant one message. design-v2
makes **`rm <message>` trash a single message by default** — the narrowest
addressable unit — and **never trashes a whole thread implicitly**. Thread-level
trash is an explicit opt-in (`rm id:thread:<id>`), so the default blast radius is
one message. This preserves gdrive-ftp's reversible-trash semantics (TRASH label,
never hard-delete) as the safety net while keeping the default minimal — no "I
lost a whole thread" moments.

## Concern (c) — `mkdir` ambiguity

**Accepted.** design-v1 left `mkdir` "dropped or remapped" and `mklabel`
"optional." design-v2 defines it explicitly: **`mkdir <name>` creates a Gmail
user label** — the faithful container-creation analog, preserving the
muscle-memory verb. Message-level membership ships as the explicit, audited
`label <message> <name>` / `unlabel <message> <name>` verbs (each with audit and
echo); no silent semantic swaps. `mklabel` is dropped as a redundant alias.

## Also folded in

- Planner concern 1 / Architect Sug 4: least-privilege scope is **locked**, not
  left open — `gmail.modify` + `gmail.compose`, never `mail.google.com`, never
  hard-delete — as a single documented constant in `auth.go`.
- Planner concern 4: a concrete v1 default page cap with a "showing N of many"
  hint so the first `ls` is a fast partial list, not a stall.
- Architect Sug 3: a dedicated `internal/gmail/model.go` home for message-name
  synthesis and its table-driven tests.

The fake-`gmailClient`-interface unit-test strategy (no live credentials), `id:`
addressing, and the explicit audited `send` verb are all retained unchanged.
