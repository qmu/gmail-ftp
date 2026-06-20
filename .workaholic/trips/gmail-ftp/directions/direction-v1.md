# Direction v1

Author: Planner
Status: draft
Reviewed-by: none yet

## Content

### 1. Value Proposition

gmail-ftp gives users a familiar, FTP-style interactive shell over their Gmail
mailbox. Instead of clicking through a web inbox, users navigate, inspect, and
manipulate email with terminal-native commands (`ls`, `cd`, `get`, `put`,
`rm`, `mv`-style verbs) mapped onto Gmail's conceptual model: labels behave
like directories, threads and messages like files, and attachments like
nested payloads.

The core value: **a non-filesystem backend (email) becomes navigable with
muscle memory users already own.** For people who live in the terminal, this
collapses the context switch between the shell and the browser. Email becomes
scriptable, pipeable, and inspectable without leaving the command line.

Who it's for: terminal-first power users, sysadmins, and automation authors
who want a low-friction, keyboard-driven way to triage, archive, fetch
attachments, and operate on mail at scale — the same audience and the same
ergonomic promise as gdrive-ftp, applied to a new backend.

### 2. Business Risk Assessment

- **Privacy and security of email access.** Email is more sensitive than file
  storage: it contains conversations, credentials, and personal data.
  *Business outcome at risk:* a single mishandling erodes trust irreparably.
  *Mitigation:* request the narrowest OAuth scopes that satisfy each command
  tier (read-only by default; mutation scopes opt-in), never persist message
  bodies beyond the session, and make destructive actions explicit and
  confirmable. Trust is the product's primary asset; protect it first.

- **User trust in destructive operations.** Unlike files, deleting or moving
  mail can be irreversible or surprising (Gmail's trash/label semantics differ
  from a filesystem). *Mitigation:* default destructive verbs to reversible
  equivalents (archive/label rather than permanent delete) and surface clear
  "what will happen" feedback. The business win is confident daily use.

- **Gmail API quotas and rate limits.** Heavy listing or bulk fetch can hit
  per-user/per-project quotas, degrading the experience. *Mitigation:* design
  command UX around batching, pagination, and graceful backoff so the product
  feels responsive rather than throttled. The outcome is sustained reliability
  under real workloads.

- **Scope creep.** Email's surface (drafts, send, filters, contacts, search
  operators) is vast; chasing parity with the full Gmail web app would dilute
  the FTP metaphor and delay delivery. *Mitigation:* v1 commits to the
  navigation-and-retrieval core that mirrors gdrive-ftp; richer mail-specific
  actions are explicitly deferred. The outcome is a shippable, coherent v1.

### 3. User Personas

- **Terminal-first power user.** Lives in tmux/vim; resents leaving the shell.
  *Job-to-be-done:* "Let me triage and pull attachments from my mailbox
  without opening a browser, using commands I already know."

- **Sysadmin / operator.** Manages system and alert mailboxes. *JTBD:*
  "Let me quickly navigate label structures, search for an alert thread, and
  fetch its contents from a remote SSH session where a GUI isn't available."

- **Automation author.** Builds scripts and pipelines. *JTBD:* "Give me a
  scriptable, predictable interface so I can fetch messages and attachments
  programmatically the same way I'd script an FTP transfer."

All three share gdrive-ftp's audience profile: they value familiarity,
keyboard speed, and a single mental model over GUI richness.

### 4. System Positioning

gmail-ftp is a **sibling product to gdrive-ftp**, not a fork in spirit but a
parallel application of the same idea to a different Google backend. gdrive-ftp
proved that an FTP-style shell can make Google Drive feel like a filesystem;
gmail-ftp extends that proven metaphor to Gmail.

The "same concept, same experience" promise means:

- **Same navigable metaphor.** Users treat labels/folders like directories,
  threads/messages like files, and attachments like file contents — `cd` into
  a label, `ls` to see threads, `get` to retrieve a message or attachment.
- **Same command vocabulary and interaction loop.** A user fluent in gdrive-ftp
  should feel at home immediately; the verbs, the prompt, and the navigation
  rhythm carry over.
- **Same directory structure and project shape.** As a sibling, it presents a
  consistent, recognizable layout so the two products feel like one family.

Where the backends differ (email has no true hierarchy; labels overlap;
threads group messages), the product makes deliberate, explainable mappings so
the metaphor stays honest rather than misleading. The differentiator from the
Gmail web app is the terminal-native, scriptable, single-mental-model
experience — not feature parity.

### 5. Business Rationale

**Why build this.** The FTP-shell-over-Google-backend pattern already has a
validated audience via gdrive-ftp. Gmail is the highest-traffic Google surface
those same users touch daily, yet it has no comparable terminal-native
experience. Building gmail-ftp extends a proven concept to a larger, stickier
use case at low conceptual risk — the metaphor and audience are already known.

**Success criteria for v1:**
- A terminal user can authenticate and reach an interactive shell over their
  Gmail mailbox.
- Core navigation works: list labels, enter a label, list threads/messages,
  and inspect a message — using the gdrive-ftp command vocabulary.
- Core retrieval works: fetch a message body and download attachments to the
  local filesystem.
- The experience is recognizably the same as gdrive-ftp for anyone who has
  used it (familiar prompt, verbs, and navigation feel).
- Default behavior is privacy-safe: least-privilege access and no surprising
  destructive actions.

**What "done" looks like for v1.** A user who knows gdrive-ftp can open
gmail-ftp, navigate their labels and threads, read messages, and pull
attachments — all from the shell — without consulting documentation, and trust
that nothing irreversible happened by accident. Richer mail-specific actions
(compose/send, filters, advanced search operators) are explicitly out of scope
for v1 and tracked as future increments.

## Review Notes

_(Placeholder — to be filled during the one-turn review round.)_
