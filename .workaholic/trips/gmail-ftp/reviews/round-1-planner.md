# Round 1 Review — Planner

Reviewer: Planner (Progressive / business-outcome lens)
Artifacts reviewed:
- `models/model-v1.md` (Architect)
- `designs/design-v1.md` (Constructor)

Coherence-checked against my own `directions/direction-v1.md`.

---

## Decision Summary

| Artifact | Decision |
| --- | --- |
| `models/model-v1.md` (Architect) | **Approve with minor suggestions** |
| `designs/design-v1.md` (Constructor) | **Request revision** |

The revision request on the Design is narrow and specific (one navigation-default
divergence plus two business-safety wordings); it is not a rejection of the
engineering plan, which is strong.

---

## Model v1 (Architect) — Approve with minor suggestions

### What serves the business outcome well

- **The "same experience" promise is honored structurally.** §1's coherence map
  ("auth, shell, audit, output, agent skill are backend-agnostic plumbing; only
  the backend-client + domain model are email-specific") is exactly the business
  guarantee from Direction §4: a gdrive-ftp user feels at home because the prompt,
  verbs, and rhythm are literally the same code. This is the single most important
  thing for the personas and the model protects it.
- **Destructive-action safety is preserved verbatim and reasoned about explicitly.**
  §3 item 5 (never make `put` send; isolate the one irreversible action behind its
  own `send` verb) and item 3's trash-is-reversible mapping match Direction §2's
  "trust is the primary asset" risk. The model treats irreversibility as a
  first-class design constraint, not an afterthought — that is the correct business
  posture for a mailbox tool.
- **`id:` promoted to canonical addressing** (§2 naming) is the right call for the
  Automation-author persona: synthesized subject-names collide, and a scriptable
  tool must have an unambiguous handle. This directly serves Direction §3's third
  persona.

### Concerns and proposals (business-outcome framing)

1. **Concern — least-privilege scope is left "open" rather than recommended firmly.**
   §4 recommends the scoped union but tags it "Flag this as a Planner/Constructor
   decision." Direction §2 makes least-privilege a *trust* commitment, not an
   optional optimization; leaving it open risks a later default to the broad
   `mail.google.com` for convenience.
   *Proposal (business outcome):* lock the recommendation now — **default to the
   narrowest scope set that covers the shipped verbs, and never request hard-delete
   capability.** Framed as an outcome: the consent screen a persona sees should ask
   for the least alarming, most explainable permission, because the first OAuth
   prompt is where trust is won or lost. I am making this a Planner ruling, not an
   open question.

2. **Concern — `label add/rm` and `send` are floated as "maybe v1, maybe later."**
   This ambiguity flows into the Design and risks scope creep (Direction §2) or, worse,
   shipping a write capability without the matching safety UX.
   *Proposal:* tie verb inclusion to the v1 success criteria in Direction §5. v1 =
   navigate + retrieve + reversible-trash. If `send`/`label` ship, they ship **with**
   their safety affordances (explicit verb, audit, recipient echo); if those
   affordances are not ready, they are deferred. The business outcome is a coherent,
   trustworthy v1 rather than a half-guarded one.

3. **Required critical concern / trade-off (raised even though approving) — the
   message-as-default-leaf choice trades discoverability for metaphor-crispness.**
   §3 item 3 picks the message as the default leaf and makes the thread opt-in. This
   is clean for the Automation persona but it removes the thing a human mailbox user
   *thinks in* — the conversation — from the default `ls`. See the cross-artifact
   navigation ruling below; my recommendation lands closer to the Constructor's
   thread tier, so I flag this as the Model's one place where metaphor-purity is
   bought at a cost to the human personas' muscle memory.

---

## Design v1 (Constructor) — Request revision

### What serves the business outcome well

- **Structural-parity discipline is the business promise made buildable.** The
  file-by-file `<same>` inventory (§1) is the most concrete possible guarantee of
  Direction §4's "same directory structure and project shape" / "feels like one
  family." Copying auth, audit, the completion machinery, and the JSON-DTO discipline
  verbatim is exactly right.
- **R5/R6 take email's higher stakes seriously.** Strengthening the README warning
  because "mailbox > drive" (R5) and gating send behind an explicit, audited verb with
  a possible `--yes` (R6) line up with Direction §2's trust-first stance.
- **The `gmailClient` interface + fake** (§3) is a quality investment that also
  serves the business: it makes the tool's behavior testable without ever touching a
  real mailbox, which protects user data during development.

### Concerns and proposals (business-outcome framing)

1. **(Revision driver) Concern — the 3-level hierarchy (label → thread → message)
   diverges from the Model, and the Design does not acknowledge or reconcile the
   divergence.** The Model (§3.3) makes the *message* the default leaf with threads
   opt-in; the Design (§ mapping table, §2 `cd` row) makes the *thread* a navigable
   directory you `cd` into and lists messages only one level deeper. These are two
   different user experiences shipping under one "same experience" banner. A reviewer
   reading both cannot tell what `ls INBOX` returns.
   *Proposal (business outcome):* converge on one default before any code is written,
   and write the chosen default into both the README command table and the SKILL.md
   so a persona's first `ls` matches the docs. My cross-artifact ruling below picks
   the Constructor's thread tier as the human-facing default, **with the Model's
   message-leaf precision preserved as the `get` target** — adopt that and the
   revision is satisfied. The Design must state the resolved default explicitly
   rather than leaving `cd <thread>` and "message is the leaf" coexisting unreconciled.

2. **Concern — `rm` defaults to trashing a *thread*, which can silently trash an
   entire conversation when the user meant one message.** §2's `rm` row says "trash a
   thread (default) or message." For a human persona, `rm <subject>` removing a
   whole multi-message conversation is a surprising blast radius — it violates
   Direction §2's "no surprising destructive actions," even though it is reversible.
   *Proposal:* make the default `rm` target the **narrowest** addressable unit in the
   current context (a message when you are inside a thread; require an explicit
   thread id / `-r`-style flag to trash a whole thread). Reversibility via TRASH is
   the safety net, but the *default blast radius* should still be minimal. Business
   outcome: confident daily use, no "I lost a whole thread" moments.

3. **Concern — `mkdir` is "dropped or remapped," and label-membership write verbs
   are half-specified.** §1/§2 leave `mkdir` ambiguous and `mklabel` "optional." For
   the gdrive-ftp muscle-memory promise, a verb that silently disappears or changes
   meaning is a per-persona surprise.
   *Proposal:* decide explicitly. Recommended for v1: keep `mkdir <name>` meaning
   "create a user label" (the faithful container-creation analog the Model endorses
   in §2), and defer message-level `label add/rm` to a later increment unless its
   audit/echo UX ships with it. Document the one chosen behavior. Outcome: the verb
   table the user already knows keeps working, with no silent semantic swaps.

4. **Required critical concern / trade-off (raised even though this is already a
   revision) — N+1 thread-metadata fetches (R2) are a latency tax that the
   Sysadmin-over-SSH persona feels most.** The mitigation (metadata format, paging,
   single-command cache) is sound, but on a large INBOX the *first* `ls` is the
   make-or-break moment for the "feels responsive" promise (Direction §2).
   *Proposal:* set a concrete v1 default page cap (e.g. first N threads) and print a
   one-line "showing N of many — use search/paging" hint, so the worst case is a fast
   partial list rather than a long stall. Outcome: the tool feels responsive on the
   exact mailboxes the operator persona points it at.

---

## Cross-Artifact Coherence Assessment

**Strong alignment on the load-bearing promises.** All three artifacts agree on:
the "same tool, for mail" value proposition (Direction §1/§4, Model §1, Design intro);
least-privilege OAuth as a trust commitment (Direction §2, Model §4, Design §2/R1);
trash-is-reversible and send-is-irreversible-so-isolate-it (Direction §2, Model §3.5,
Design R6); and `id:` as the unambiguous escape hatch for automation (all three). This
is a coherent plan — the disagreements are about *navigation shape*, not direction.

**The one material incoherence — navigation default.** Architect: message is the
default leaf, thread opt-in (2 conceptual levels). Constructor: label → thread →
message is a navigable 3-level tree you `cd` through. My Direction §4 promised "`cd`
into a label, `ls` to see threads, `get` to retrieve a message" — which is in fact
*closer to the Constructor's* model. This divergence must be resolved before coding
(see ruling) or the README, SKILL.md, and the user's first `ls` will not agree.

**Secondary coherence note — `rm` blast radius and verb inclusion** are described
slightly differently across Model (§3.6 message-vs-label) and Design (§2 thread-vs-
message default). Resolving the navigation default also forces these to settle,
because the default `rm` target should follow the default navigation unit.

---

## Navigation-Default Ruling (the requested divergence)

**Recommendation: adopt the Constructor's label → thread → message hierarchy as the
human-facing navigation default, but keep the Architect's message-as-`get`-leaf
precision. Threads are the directory a human `cd`s into and `ls` lists; the message
is the file you `get`.**

Rationale, from the persona / muscle-memory lens:

- **It matches the muscle memory the product is selling.** gdrive-ftp is
  root → drive → folder → file. The Constructor's label → thread → message reproduces
  that *depth and rhythm* one-for-one: the human `cd`s down a level, then `ls` reveals
  the leaves. The Architect's flat 2-level model is *cleaner as a metaphor* but
  *shallower than the tool it imitates*, so a gdrive-ftp user's hands expect one more
  level than they get. Direction §4's whole promise is "same navigation rhythm."

- **The thread is the unit humans actually think in.** The Terminal-first and Sysadmin
  personas (Direction §3) reason about "the alert conversation" or "the receipt
  thread," not about message N within it. Making the thread the navigable directory
  means the default `ls INBOX` shows conversations — the same granularity Gmail's own
  web UI shows — which is the least surprising default for a human triaging mail.

- **The Architect's concern (a two-kinds-of-file listing is ambiguous) is fully
  resolved by the tier, not contradicted by it.** In the 3-level model, a label lists
  *only threads*, a thread lists *only messages (+ attachments)* — each `ls` returns
  exactly one kind, which is *more* metaphor-crisp than mixing messages and a
  thread-grouping toggle at one level. So we get the Architect's crispness goal via
  the Constructor's structure.

- **The Architect's strongest point — the message is the file-like unit you `get` as
  one `.eml`** — is preserved exactly: the leaf you `get` is still the message; the
  thread is just the folder above it. The Automation persona keeps message-level and
  `id:` addressing unchanged. Offer `get` on a thread as the opt-in `.mbox` export the
  Architect proposed, so power users lose nothing.

**Net:** Constructor's depth, Architect's leaf semantics. This is the only
combination that delivers both "same muscle memory" (depth/rhythm) and "crisp
metaphor" (one kind per `ls`, message-as-file) without forcing the human personas to
think in opt-in toggles. Both artifacts should state this resolved default verbatim
in README and SKILL.md.

## Review Notes

(placeholder — author responses appended in Step 3)
