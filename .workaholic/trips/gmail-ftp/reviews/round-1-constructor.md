# Round 1 Review — Constructor

Reviewer: Constructor (Conservative; engineering-quality / production-readiness lens)

Artifacts reviewed:
- `directions/direction-v1.md` (Planner)
- `models/model-v1.md` (Architect)

Cross-checked against my own `designs/design-v1.md` and the reference codebase
`/home/ec2-user/projects/gdrive-ftp`, plus the real `google.golang.org/api/gmail/v1`
package (`@v0.284.0`) in the module cache.

---

## Decision summary

| Artifact | Decision |
| --- | --- |
| `direction-v1.md` (Planner) | **Approve with minor suggestions** |
| `model-v1.md` (Architect) | **Request revision** |

The model is request-revision **not** because it is wrong — it is the strongest
of the three on API surface fidelity — but because its central navigation default
contradicts both the Direction's stated success criteria and my Design, and the
contradiction is load-bearing for testability and quota behavior. One concrete
fact decides it (see Navigation, below), and the model should be amended to
record that decision rather than leave it as an open question.

---

## API-implementability check (build lens)

I verified the model's and direction's claims against the real Gmail v1 package.

**What checks out:**
- `Users.Threads.List` supports `.LabelIds(...)` and `.Q(...)`
  (`gmail-gen.go:11110`, `:11134`) — label-scoped listing and search are real.
- `Users.Messages.Get` and `Users.Threads.Get` support `.Format(...)` and
  `.MetadataHeaders(...)` (`:4584`, `:10983`) — the metadata-only optimization
  the Design's R2 relies on is a real lever.
- `users.labels` / `users.messages` / `users.messages.attachments` /
  `users.threads` / `users.drafts` / `users.messages.send` all exist as the model
  asserts. The "thin FTP-flavored wrapper" is implementable.
- The reference `Ref`-stack cwd (`internal/shell/shell.go:36`, `cwd []gdrive.Ref`)
  is depth-agnostic, so *either* a 2-level (label→message) *or* a 3-level
  (label→thread→message) hierarchy is mechanically supportable without
  re-architecting the shell. Good news for both proposals.

**The one fact that changes the analysis (and that BOTH artifacts underweight):**
`threads.list` returns thread resources that contain **only** `id`, `historyId`,
and `snippet` — **no subject and no `messages`** (verified at `gmail-gen.go:2118`
`type Thread`, and the API note at `:1602` `ListThreadsResponse`: *"each thread
resource does not contain a list of messages"*). Likewise `messages.list` returns
only `{id, threadId}`. **Therefore any listing that shows a human-readable name
(subject/from/date) is an unavoidable N+1: one `list` call plus one
`get(format=metadata)` per row.** This is true for *both* navigation models, but
it compounds differently for each (see below). The Direction's quota risk (§2) and
the Model's §3 item 3 both treat this as a modeling nuance; it is actually the
dominant cost-and-latency driver of the whole product and must be designed for
explicitly.

---

## Navigation divergence — the requested recommendation

**The split:** the Architect makes the **message the default leaf directly under a
label** (label → message, two levels; thread is opt-in via a `threadId` field and
`id:thread:<id>`). My Design uses **label → thread → message/attachment** (three
levels). The Direction's success criteria say "enter a label, list
threads/messages, and inspect a message" — deliberately hedged, so it does not
break the tie.

**Recommendation: adopt the Architect's default — message is the default leaf
under a label (2-level), thread is opt-in.** I am recommending *against my own
Design's 3-level default. Rationale, from the implementation-complexity and
testability lens:

1. **Quota/latency parity, not advantage.** I initially favored 3-level on the
   theory that a label lists few threads and we expand on demand. But the verified
   API shape removes that advantage: whether the leaf is a thread or a message,
   the listing is `list` + N×`get(metadata)`. The 3-level model does not save
   calls at the label level (a label still contains ~the same number of threads as
   messages for triage-sized views) and *adds* a second N+1 when you `cd` into a
   thread and expand its messages. 2-level has one N+1 tier; 3-level has two.

2. **Resolver/testability surface is smaller.** In 2-level, `Ref.Kind ∈ {label,
   message}` and the resolver answers one question per component. In 3-level,
   `Kind ∈ {label, thread, message}` and the resolver must disambiguate "is this
   path component a thread or a message?" at the same depth — exactly the
   ambiguous, non-unique-subject case my own R3 flags. Fewer kinds = fewer
   `ErrAmbiguous` branches = a smaller, more deterministic unit-test matrix for
   the fake-client tests I committed to in Design §3.

3. **The metaphor stays crisp.** A single-message thread (the common case for
   triage) becomes a pointless `cd thread` → `ls` → one message in the 3-level
   model. The 2-level model shows that message directly; threads with >1 message
   are the exception surfaced via `id:thread:` and a `--threads`/grouping option.
   This matches the Direction's "navigate without consulting documentation."

4. **`get` semantics are cleaner.** The most file-like, byte-downloadable unit is
   a single message (`get` → `.eml`). Making that the default leaf means `get`
   always has an unambiguous target. In 3-level, `get <thread>` has to choose
   between an `.mbox` of the whole thread vs. erroring — extra surface.

**Concrete proposal to reconcile:** keep the Architect's 2-level default, but
adopt three things from my Design to preserve the quality bar:
   (a) `threadId` on every DTO row (Architect already has this) **plus** an
       `id:thread:<id>` addressable container and a `get id:thread:<id>` → `.mbox`
       export for the multi-message case;
   (b) a `--threads` / per-label grouping flag so power users get thread rollups
       on demand without making it the default;
   (c) my fake-`gmailClient`-interface test strategy (Design §3) applied to the
       2-level resolver so `cmdLs`/`cmdCd`/`cmdGet` are unit-tested without live
       creds.
   I will revise `design-v2.md` to the 2-level default if the team accepts; the
   Architect should record this as the *resolved* answer to its open question
   "Default leaf granularity (message vs thread)" rather than leaving it open.

---

## `model-v1.md` (Architect) — Request revision

Strong artifact: the per-part mapping table, the strain analysis (§3), and the
boundary-integrity section are accurate against the real API and align tightly
with my Design. Reasons for *request revision* (each with a proposal):

- **C1 — N+1 cost is under-modeled (must-fix).** §3 item 3 treats thread-vs-message
  as a metaphor-clarity issue; the verified `threads.list`/`messages.list` shape
  (IDs/snippet only) makes per-row metadata fetching the product's dominant cost.
  *Proposal:* add a sub-section to §3/§4 stating the N+1 explicitly and committing
  to `get(format=metadata, metadataHeaders=Subject,From,Date)`, a capped default
  page size, and intra-command metadata caching — and fold this into the
  navigation decision (2-level minimizes the number of N+1 tiers).

- **C2 — open questions should be closed by the model, not punted (must-fix).** §5
  leaves four decisions "for Direction/Design to resolve," including the leaf
  granularity that *this review resolves above* and the scope choice. A model that
  defers its own central modeling decision is not yet a buildable bridge.
  *Proposal:* in v2, record the resolved answers (2-level default per above;
  scope = `gmail.modify` + `gmail.compose`, never full `https://mail.google.com/`,
  matching my Design §2/R1) and demote the rest to "v1 vs. later" sequencing.

- **C3 — `label`/`send` verbs are additions beyond the gdrive-ftp verb set
  (minor).** §3 items 2/5 propose new verbs but flag the ship/defer decision as
  someone else's. *Proposal:* recommend v1 = read + trash + draft (no `send`, no
  `label` mutation) to honor the Direction's "shippable, coherent v1" and isolate
  the one irreversible action (`send`) to a v1.1 increment behind an explicit verb
  + confirmation. This keeps the v1 scope testable without send/label side effects.

- **C4 — coherence with my Design on one naming point (minor).** My Design's
  mapping table currently shows thread-as-folder (3-level); if the team accepts
  the 2-level recommendation, the Architect's table is the correct one and I will
  align `design-v2.md` to it. Flagging so the two artifacts converge rather than
  ship contradictory hierarchies.

---

## `direction-v1.md` (Planner) — Approve with minor suggestions

The business framing is sound, the privacy/least-privilege stance is exactly right
for a restricted-scope Gmail product, and the "sibling to gdrive-ftp" positioning
gives the build a clear quality target. Minor suggestions (each constructive):

- **S1 — success criteria should name a credential-free acceptance path.** "A
  terminal user can authenticate and reach an interactive shell" is only
  E2E-verifiable with a live Google account, which the team cannot do in CI.
  *Proposal:* add a v1 success criterion that the *navigation/retrieval logic* is
  verifiable via unit tests against synthetic Gmail structs (my Design §3), so
  "done" has an objective, credential-free gate in addition to the live smoke test.

- **S2 — "list threads/messages" hedge should be decided.** The criteria say
  "threads/messages," leaving the leaf granularity open at the business level.
  *Proposal:* once the team accepts the 2-level recommendation, restate the
  criterion as "list messages within a label (threads addressable on demand)" so
  the business definition of done matches the build.

- **S3 — quota/latency is a UX promise, not just a risk.** §2 lists quotas as a
  risk; given the verified N+1, a large mailbox *will* show a visible pause on
  first listing. *Proposal:* set the expectation in the success criteria/README
  language ("brief pause on large labels," mirroring gdrive-ftp) so responsiveness
  is a stated, testable bar rather than an implicit one.

---

## Cross-artifact coherence

- **Auth / boundary discipline:** all three agree — reuse `auth.go` verbatim,
  change only the scope, never marshal the vendor struct. Coherent and correct.
- **Scope:** Direction (least-privilege, opt-in mutation), Model (recommends scoped
  union over full `mail.google.com`), and my Design (`gmail.modify` +
  `gmail.compose`) **converge**. The Model should just record this as decided.
- **Safety culture:** trash-not-delete, `put`-never-sends, explicit `send` —
  unanimous across all three. Strongest point of coherence.
- **The one real divergence is navigation granularity** (Model 2-level vs. Design
  3-level), with the Direction deliberately neutral. Resolved above in favor of
  2-level. Once the Model closes its open questions and my Design aligns to
  2-level in v2, the three artifacts are coherent and buildable.

## At-least-one-concern (even on the approval)

Even for the approved Direction: success criterion "reach an interactive shell
over their Gmail mailbox" is not independently verifiable by this team without
live credentials — S1's credential-free unit-test gate is the proposal that keeps
"done" objective.
