---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure, Domain]
effort:
commit_hash: 00d7111
category: Added
depends_on: [20260622214650-t13-driver-contract-trait.md, 20260622214650-t15-codec-registry-decode-encode.md]
---

# Driver: git (object model, not the GitHub API)

## Overview
This ticket implements the `git` driver — the canonical proof of the RFD's claim that a
single driver can expose **all four archetypes** on different sub-paths (RFD §5, §2.1).
It reads and writes the git **object model directly** (loose objects + packs, refs, reflog),
**not** the GitHub HTTP API (that is a separate `github` object-graph driver). It is the
showcase for the `@version` temporal coordinate (RFD §4): `/git/<repo>@<ref>/<path>` lets an
agent `cat`/`ls` any blob or tree at any commit. It is the showcase for "git as SQL"
(RFD §1 lineage: Mergestat/askgit): `/git/<repo>/commits`, `/changes`, `/blame` are
relational nodes you query with `FROM … |> WHERE … |> JOIN …`. It demonstrates the
"path is the type, CRUD is universal" rule (RFD §3): a git commit is
`INSERT INTO /git/<repo>/commits` — deliberately avoiding a `COMMIT` keyword clash with the
frozen Plan keyword `COMMIT`. Irreducible transitions (`merge`, `rebase`, `checkout`, `tag`)
are namespaced **procedures** under `CALL git.*`. Because git objects are content-addressed and
immutable, every write is unusually safe: nothing is overwritten, and reflog gives free
recovery — making this the driver where the effects-as-data + audit-ledger story is cleanest.

## Scope
In scope:
- Read-only versioned-blob FS: `ls`/`SELECT` over `/git/<repo>@<ref>/<path>` (trees, blobs).
- Relational history nodes: `/git/<repo>/commits`, `/changes`, `/blame`, `/refs`, `/tags`.
- Mutable refs: `UPDATE /git/<repo>/refs` to move a branch / set a ref (fast-forward + forced).
- Commit creation as `INSERT INTO /git/<repo>/commits` (build tree → commit object → ref update).
- Procedures: `CALL git.merge / rebase / checkout / tag` returning effect-plan nodes.
- Reflog-backed recovery surface: `/git/<repo>/reflog` (read) + recovery in partial-failure paths.
- `Driver` impl wired to the contract trait (t13) and codec interop (t15) for blob↔rows.

Out of scope (deferred):
- GitHub REST/GraphQL, PRs, CI dispatch → `github` driver ticket (object-graph archetype).
- Remote transport (fetch/push over ssh/https) → deferred git-remote ticket; this ticket is
  **local repo objects only**.
- Submodules, LFS, sparse/partial clone, worktrees → deferred.
- Server bindings (`TRIGGER ON git-change`, pollers) → E7 server tickets.
- Federation pushdown planner internals → E3; this ticket only declares pushdown capability.

## Key components
New crate `crates/driver-git` (`qfs-driver-git`), thin over `gix` (gitoxide) for object
access — no shelling out to `git`, no vendor types past the boundary (RFD §9 owned DTOs).

- `GitDriver` implementing the `Driver` trait from t13:
  ```rust
  pub struct GitDriver { repos: RepoResolver, caps: Capabilities }
  impl Driver for GitDriver {
      fn namespace(&self) -> Namespace;            // path tree + per-node archetype + schema
      fn capabilities(&self, node: &PathNode) -> NodeCaps;  // parse-time verb gating (RFD §5)
      fn plan_read(&self, q: &Query) -> Result<PlanNode, DriverError>;
      fn plan_write(&self, w: &WriteOp) -> Result<PlanNode, DriverError>; // pure: builds effects
      fn procedures(&self) -> &[ProcDecl];         // git.merge / rebase / checkout / tag
      fn pushdown(&self, sub: &PipeTree) -> Pushdown;
  }
  ```
- `path::GitPath` — parses `/git/<repo>[@<ref>]/<rest>` into an owned DTO; resolves `<ref>`
  (branch/tag/sha/`HEAD~n`) to an `ObjectId`. `@<ref>` is the §4 temporal coordinate.
- Archetype nodes (each maps to a frozen-grammar shape; **zero new keywords**, RFD §3):
  - `BlobFs` (Blob/namespace): `ls`/`cp(read)` over tree entries; blob bytes feed the codec
    registry (t15) so `DECODE json|yaml|toml|csv|md+frontmatter` works on any committed file.
  - `Commits` (Relational): columns `sha, parents[], author, committer, time, message,
    tree`; supports `SELECT/WHERE/ORDER BY/LIMIT/JOIN` and `INSERT INTO` (= make a commit).
  - `Changes` (Relational): exploded per-file diff rows `(sha, path, status, +lines, -lines)`
    — `git log --name-status` as a table, JOINable to `commits`.
  - `Blame` (Relational): `(path, line, sha, author, time)`.
  - `Refs` / `Tags` (mutable pointers): `SELECT` + `UPDATE` (move/create/delete a ref).
  - `Reflog` (Append/log): tail `SELECT`; the recovery oracle.
- `effects::GitEffect` (effects-as-data, RFD §6) enum: `WriteLooseObject{oid,kind,bytes}`,
  `UpdateRef{name, old: Option<Oid>, new: Oid, force}`, `WriteReflogEntry{…}`. Each carries
  `irreversible: bool` (object writes are reversible/GC-able → `false`; forced ref moves that
  orphan history → flagged, but reflog-recoverable). Plans are built purely; only `COMMIT`
  applies them via the interpreter (RFD §3 purity invariant: every fn `… -> Plan`).
- `procs::{merge, rebase, checkout, tag}` — each returns a `PlanNode` (a DAG of `GitEffect`s),
  never performs I/O. `git.merge` ≠ `github.merge` by namespace (RFD §3).
- `dto.rs` — owned column DTOs (`CommitRow`, `ChangeRow`, `BlameRow`, `RefRow`); `gix` types
  never escape the crate.
- `caps.rs` — declares per-node `NodeCaps` so unsupported verbs (e.g. `UPDATE /commits`) are
  rejected at **parse time** with a structured error (RFD §5, important for the AI).

## Implementation steps
1. Scaffold `crates/driver-git`; add `gix` (object DB, refs, reflog) as the only heavy dep.
2. Implement `GitPath` parsing + `<ref>`→`ObjectId` resolution (branch/tag/sha/`HEAD~n`/`@`).
3. Build `Namespace` + per-node `Capabilities`; register archetype/schema for `DESCRIBE`.
4. Implement `BlobFs` read: tree walk → `ls`; blob read → bytes; hand bytes to codec registry.
5. Implement `Commits`/`Changes`/`Blame` readers producing owned row DTOs; wire `WHERE/ORDER
   BY/LIMIT` pushdown where `gix` revwalk supports it; fall back to local filter otherwise.
6. Implement `Refs`/`Tags` `SELECT` + `UPDATE` → `UpdateRef` effect (with old-oid CAS, RFD §6
   optimistic concurrency via `@version`).
7. Implement `INSERT INTO /commits`: build tree from staged rows/blobs → write tree + commit
   objects (`WriteLooseObject`) → `UpdateRef` HEAD/branch; all as pure plan nodes.
8. Implement `procs::{merge,rebase,checkout,tag}` as plan constructors; declare them in
   `procedures()` so `CALL` resolves only these (capability).
9. Implement the `Apply` path the interpreter calls on `COMMIT`: write objects, then refs,
   then reflog entry; emit audit-ledger records (RFD §6/§10).
10. Implement `Reflog` reader + a `recover` helper used by partial-failure recovery.
11. Register `GitDriver` with the driver registry behind a `git` capability flag.
12. Tests + clippy + docs (DESCRIBE snapshot of the namespace).

## Considerations
- **Least privilege & secrets (RFD §10):** local object model needs no network tokens —
  a deliberate security win. Repo root must be capability-gated and path-canonicalized to
  prevent traversal outside the mounted repo; never read credentials or `.git/config` secrets
  into rows.
- **Idempotency / recovery (RFD §6):** object writes are content-addressed → naturally
  idempotent (writing an existing oid is a no-op). Ref moves use **compare-and-swap on the old
  oid** (optimistic concurrency); on conflict, reject rather than clobber. Every applied effect
  appends a reflog entry, so `Reflog` + the audit ledger reconstruct any state — partial failure
  (objects written, ref move failed) is safe and re-runnable.
- **Hard part — keyword clash:** a git "commit" must NOT touch the frozen `COMMIT` plan
  keyword. Resolve by modeling commit creation strictly as `INSERT INTO …/commits`; document
  this explicitly in the driver schema and DESCRIBE output so the AI never emits `COMMIT` for
  it.
- **Hard part — merge/rebase as pure plans:** these can conflict. The proc must compute the
  result tree *during planning* (in-memory three-way merge) and surface conflicts as a typed
  plan-build error in PREVIEW, never as a half-applied mutation. Non-conflicting results
  reduce to `WriteLooseObject` + `UpdateRef` effects.
- **Hard part — diff/blame cost:** `/changes` and `/blame` over deep history are expensive;
  require/strongly-prefer a bounding predicate (ref range / `LIMIT`) and push the revwalk down
  to `gix`; bound work and time out per RFD §6 observability.
- **Observability (RFD §6/§10):** structured logs per object/ref op; audit ledger is the
  applied-effect record; PREVIEW prints object count + ref before→after.
- **Coding standards (RFD §9):** owned DTOs only, no `gix` types past the boundary; consumer
  small-trait `Driver`/`Codec`; enums for archetypes/effects; crate self-contained.

## Acceptance criteria
- `cargo build` and `cargo clippy --all-targets -- -D warnings` are green for `driver-git`.
- Unit tests over a fixture repo (committed test data, **no live creds, no network**):
  - `FROM /git/<repo>@<ref>/<path>` cats the exact bytes of that blob at that commit; `ls`
    of a tree path lists entries; reading a committed `*.md` through the codec registry yields
    frontmatter columns + `body` (t15 interop).
  - `FROM /git/<repo>/commits |> WHERE author = '…' |> ORDER BY time |> LIMIT n` returns
    expected rows; a `commits JOIN changes` query returns per-file change rows.
  - `/blame` returns correct `(line, sha, author)` for a known file.
- **Plan assertions (preferred for write paths):**
  - `INSERT INTO /git/<repo>/commits …` under PREVIEW yields a plan of `WriteLooseObject` +
    `UpdateRef` effects with correct old/new oids and **applies nothing**; after `COMMIT`, the
    branch points at the new commit and a reflog entry exists.
  - `UPDATE /git/<repo>/refs … ` with a stale old-oid is **rejected** (CAS), not clobbered.
  - `CALL git.merge(...)` on a conflicting input fails at plan-build (PREVIEW) with a typed
    error and zero effects; a clean merge produces the expected effect DAG.
- Unsupported verb on a node (e.g. `UPDATE /git/<repo>/commits`) is rejected at **parse time**
  with a structured capability error.
- A forced ref move is recoverable: after the move, `/git/<repo>/reflog` shows the prior oid
  and the recovery helper restores it.
- `DESCRIBE /git/<repo>/...` golden test matches the declared namespace/schema/archetypes.
