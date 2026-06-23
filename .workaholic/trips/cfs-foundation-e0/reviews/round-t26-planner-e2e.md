# Round t26 — Planner E2E (git object-model driver)

- Author: Planner (Progressive)
- Role: E2E / external-interface testing (black box) — no code review, no reliance on the
  Constructor's unit tests as the deliverable
- Ticket: `.workaholic/tickets/todo/a-qmu-jp/20260622214650-t26-driver-git-object-model.md`
- Under test: Constructor t26 (commit `5f5ce18` + required-fix `a870831`); Architect-approved
  (`698f1a5`, `f626d5a`)
- Crate: `crates/driver-git/` (`cfs-driver-git`)
- Harness (Planner-owned, NEW): `crates/driver-git/tests/e2e_planner.rs` — an integration-test
  target that drives the **public crate surface only** over a Planner-built fixture (not a reuse of
  `src/tests.rs`), plus a bonus scenario that shells out to the **local `git` binary** (v2.50.1) in
  a tempdir to inflate canonical loose objects through the public surface. No network, no creds.

## Decision (round 2 — FINAL)

**Approve with observations — 9/9 scenarios PASS. The round-1 blocker is CLOSED.**

The Constructor's additive fix (commit `74b7ad9`) added the public builder
`CommitInput::new(branch, author, committer, message).at_time(t).with_file(path, bytes)` /
`.with_files(map)` — `#[non_exhaustive]` retained, `CommitInput`/`CommitPlan` shapes unchanged. The
previously-unreachable INSERT-INTO-commits write path is now callable from OUTSIDE the crate, so
scenarios 3 and 8's plan half are closed through that public constructor.

- E2E harness: **25 passed; 0 failed** (`cargo test -p cfs-driver-git --test e2e_planner`).
- Full workspace: **793 passed; 0 failed** (`cargo test --workspace`).
- Gates green: `cargo clippy --workspace --all-targets -- -D warnings` → exit 0;
  `cargo fmt --all --check` → exit 0. The harness keeps the conventional allow header.

### Round-2 added/closed coverage

- **Scenario 3 (PREVIEW vs COMMIT) — CLOSED.** `write_insert_commit_preview_is_side_effect_free`:
  `plan_insert_commit` over a publicly-built `CommitInput` yields a plan with `WriteLooseObject`
  (Insert) + `UpdateRef` (Update) effects, the CAS old-oid = the current tip (c2), the new-commit
  oid is content-addressed (≠ c2), and PREVIEW applies NOTHING (branch still c2, reflog empty,
  re-plan deterministic). `write_commit_moves_branch_and_writes_reflog`: after applying through
  `apply_shared`, the branch points at the new commit and the reflog records the move from c2;
  re-applying the **object-write** effects is a content-addressed no-op (`affected == 0`).
  `write_commit_runtime_bridge_constructs_over_a_real_commit_plan`: the runtime bridge constructs
  and the shared applier drives a real commit plan.
- **Scenario 8 (plan-node cross-check) — CLOSED.**
  `keyword_clash_commit_creation_is_insert_not_the_commit_keyword` now asserts BOTH halves: the
  capability half (INSERT allowed on `/commits`, UPDATE rejected) AND the plan-node half — a real
  commit-creation plan is built ENTIRELY from INSERT/UPDATE effect kinds; the frozen `COMMIT` plan
  keyword is never an effect-node verb (it stays exclusively the interpreter's apply verb).

### Round-2 harness correction (Planner-side, not a driver defect)

My round-1-proposed idempotency-by-full-replay assertion was incorrect: re-applying a commit plan's
CAS `UpdateRef` after the ref has already moved CORRECTLY conflicts (its `old` is now stale) — that
is the very CAS safety property scenario 4 validates, not a bug. I corrected the harness to assert
idempotency only on the content-addressed object writes (re-applying a `WriteLooseObject` for an
existing oid → `affected == 0`), which jointly confirms both true properties: object writes are
idempotent AND the CAS ref move is not replay-idempotent (by design). No driver change was needed.

---

## Decision (round 1 — historical, now superseded)

**Request revision — blocked, 7/9 scenarios confirmed PASS; scenarios 3 and 8 were BLOCKED at the
external interface by a `#[non_exhaustive]` DTO with no public constructor.**

22 black-box tests passed at round 1 (`cargo test -p cfs-driver-git --test e2e_planner` →
`22 passed; 0 failed`). Gates were green. The blocker was genuine and in the Constructor's lane
(`planner.rs`); per charter the Planner did NOT fix it — it was handed back and fixed in `74b7ad9`.

## Scenario results (ran / expected / actual)

| # | Scenario | Result | Test(s) |
|---|----------|--------|---------|
| 1 | BlobFs read at a ref (exact bytes, ls, md→codec frontmatter+body) | PASS | `blobfs_cat_at_ref_returns_exact_bytes`, `blobfs_ls_lists_tree_entries`, `blobfs_md_through_codec_registry_yields_frontmatter_columns_and_body` |
| 2 | Relational (commits WHERE/ORDER/LIMIT, commits⋈changes, blame, refs/tags, reflog) | PASS | `relational_commits_where_order_limit`, `relational_commits_join_changes_per_file_rows`, `relational_blame_attributes_line_to_last_touch`, `relational_refs_and_tags_rows`, `relational_reflog_tail_newest_first` |
| 3 | Write plans PREVIEW vs COMMIT (INSERT INTO /commits) | round 1: BLOCKED → **round 2: PASS** | `write_insert_commit_preview_is_side_effect_free`, `write_commit_moves_branch_and_writes_reflog`, `write_commit_runtime_bridge_constructs_over_a_real_commit_plan` |
| 4 | CAS ref update — stale old-oid rejected, not clobbered | PASS | `cas_stale_old_oid_is_rejected_not_clobbered`, `cas_fresh_old_oid_is_accepted`, `cas_tag_creation_rejects_existing_ref` |
| 5 | merge-conflict purity (zero effects) + clean-merge DAG | PASS | `merge_conflict_is_plan_build_error_with_zero_effects`, `merge_clean_produces_expected_effect_dag`, `merge_rebase_shares_the_zero_effect_conflict_surface` |
| 6 | Capability gating at PARSE/resolve time | PASS | `capability_update_on_commits_rejected_at_parse_time`, `capability_per_node_matrix_holds`, `capability_call_resolves_only_declared_git_procedures` |
| 7 | Reflog recovery of a forced ref move | PASS | `reflog_forced_move_is_recoverable` |
| 8 | COMMIT keyword-clash (commit = INSERT, never `COMMIT` kw) | round 1: PARTIAL → **round 2: PASS** | `keyword_clash_commit_creation_is_insert_not_the_commit_keyword` (capability + plan-node halves), `keyword_clash_describe_documents_commits_node` |
| 9 | (Bonus) real `git` loose object inflated through public surface | PASS | `real_git_loose_object_inflates_through_public_surface` |
| + | Cross-cutting: checkout proc plans a reflog-recorded HEAD move | PASS | `checkout_proc_plans_a_reflog_recorded_head_move` |

### Detail on the PASS scenarios

1. **BlobFs at a ref.** `blobfs::cat(repo, "main", "config.toml")` returns the EXACT committed
   bytes `name = "demo"\nport = 9090\n`; the same file at `@<c1-sha>`, at the `rel-1` tag, and at
   `main~1` all return the v1 bytes `…port = 8080\n` — confirming the §4 `@<ref>` temporal
   coordinate (branch / 40-hex-sha / tag / `~n` ancestor). `blobfs::ls` lists `[README.md,
   config.toml]` name-sorted. `blobfs::cat_decode` through the t15 `MarkdownFrontmatterCodec` yields
   columns `title`, `version`, `body`, and `body` carries `# Demo … The body text.`.

2. **Relational.** `relational::commits` returns `[c2, c1]` newest-first; `LIMIT 1` bounds the walk
   to `c2`; the `author='Grace…'`/`'Ada…'` residual filters to exactly one row each. `commits ⋈
   changes ON sha`: c2 has one `M config.toml` row, c1 has two `A` rows (`README.md`, `config.toml`),
   and every change row joins a commit row. `/blame config.toml` attributes both lines to c2
   (Grace, the last touch); `/blame README.md` attributes to c1 (never changed after root).
   `/refs` lists `refs/heads/main@c2`; `/tags` is the `refs/tags/%` residual → `rel-1@c1`. `/reflog`
   tails newest-first (`c1→c2` heads it).

4. **CAS.** A stale-old-oid `UPDATE refs/heads/main` (expecting c1 while it is c2) is rejected as a
   typed `conflict` and the branch is NOT clobbered (still c2). A fresh-old-oid move (expecting c2)
   is accepted. A `plan_tag("rel-1", …)` creation (old=None) over an existing tag is rejected as a
   `conflict`, leaving the existing tag at c1. This is the concurrent-style stale-write break.

5. **Merge purity (highest risk).** `plan_merge` with base=c1/ours=c2(port 9090)/theirs=c3(port
   7070) — both sides diverged from base to different content — returns `Err(merge_conflict)`. Because
   the API returns `Result<Plan, …>`, a conflict yields **no `Plan` at all**, so there is literally
   nothing to apply: the zero-effect guarantee is structural, not a post-hoc filter. A clean merge
   (ours unchanged from base, theirs changed) returns a Plan with ≥3 nodes including an `Update`
   (UpdateRef) and `Insert` (object writes). `plan_rebase` shares the identical zero-effect conflict
   surface (the documented E0 delegation-to-merge park).

6. **Capability gating (parse/resolve time).** `check_capability(&driver, "/git/demo/commits",
   Update)` → `unsupported_verb` BEFORE any plan exists; `Insert`/`Select` allowed; `Remove`
   rejected. Per-node matrix holds: refs/tags = {Select,Update}; blob = read-only (Update/Insert
   rejected); changes/blame/reflog = Select-only. `resolve_proc` resolves only `merge/rebase/
   checkout/tag` (all `irreversible=false`); `force_push` → `unknown_procedure`.

7. **Reflog recovery.** A forced `UPDATE refs/heads/main` from c2→c1 (orphaning c2) succeeds; the
   reflog's newest entry records `old=c2`; `recover_ref` restores the branch to c2 from the reflog.

9. **(Bonus) real git inflate end-to-end.** A real repo built with `git` 2.50.1 in a tempdir writes
   a `doc.md` frontmatter file. Its on-disk COMPRESSED loose objects (commit, tree, blob — verified
   first byte `0x78`, a real zlib stream) are loaded VERBATIM via `LooseObjectDb::insert_loose`,
   then read back through the public `Repo`/`blobfs::cat`/`blobfs::ls`/`cat_decode` surface. The
   in-house DEFLATE inflater reproduces the EXACT committed bytes, the codec decodes the frontmatter
   (`title`, `n`, `body`), and the in-house content-address of `doc.md` equals the oid canonical
   `git hash-object` computed. This independently confirms the ADR-0003 in-house reader against
   canonical git output from the OUTSIDE — strengthening the Architect's closed finding.

## BLOCKER — scenarios 3 and 8 are uncallable from outside the crate

**Finding.** The sole commit-creation write entry point is
`plan_insert_commit(repo_name, repo: &Repo, input: &CommitInput) -> Result<CommitPlan, GitError>`.
Its input `CommitInput` (`crates/driver-git/src/planner.rs:146`) is declared `#[non_exhaustive]`
and exposes **no public constructor or builder**. An out-of-crate caller therefore cannot construct
the argument:

```rust
let input = cfs_driver_git::CommitInput {
    branch: …, author: …, committer: …, time: …, message: …, files: …,
};
// error[E0639]: cannot create non-exhaustive struct using struct expression
```

I verified this with a minimal external reproduction (a throwaway `tests/_blocker_probe.rs` that
only constructs `CommitInput`) — it fails to compile with E0639. `#[non_exhaustive]` does not apply
**within** the defining crate, which is why the Constructor's internal `src/tests.rs` constructs
`CommitInput` directly and its unit tests pass: **the unit suite cannot observe this gap; only a
black-box harness can.** This is exactly the differentiation the protocol assigns the Planner.

**Impact (business + correctness).**
- Scenario 3 (PREVIEW vs COMMIT over an INSERT) and scenario 8's plan-node-kind cross-check have
  **no reachable external entry point**. The INSERT-INTO-commits write path — the RFD §3
  COMMIT-keyword-clash centerpiece and the showcase write of the whole driver — is not callable by
  the engine that will wire an `INSERT INTO /git/<repo>/commits` evaluation through this function.
- This is not a test-only inconvenience: any real consumer (the t10 interpreter's INSERT evaluator,
  a CLI, the federation layer) hits the same wall.

**What is NOT blocked (so the write substrate is still validated end-to-end).** The CAS / forced-move
/ tag / checkout write paths go through `plan_update_ref` / `plan_tag` / `plan_checkout`, which take
plain `Oid`/`&str` args (no `CommitInput`). Scenarios 4, 7, and the checkout cross-cut therefore
exercise the full effects-as-data → `apply_shared` → CAS → reflog → recovery substrate from the
outside and PASS. The keyword-clash CAPABILITY contract (scenario 8's AI-facing half) is also
reachable and PASSES via `check_capability`. So the gap is narrowly the `CommitInput` DTO
ergonomics, not the apply engine.

**Proposed fix (Constructor's lane — `planner.rs`).** Add a public constructor or `#[must_use]`
builder for `CommitInput`, mirroring the builder idiom the rest of the crate's owned DTOs already
use (`ProcSig::new(...).with_params(...)`, `RepoResolver::with_repo`, `GitApplier::with_store`),
e.g.:

```rust
impl CommitInput {
    pub fn new(branch, author, committer, time, message) -> Self { … files: BTreeMap::new() }
    #[must_use] pub fn with_files(mut self, files: BTreeMap<String, Vec<u8>>) -> Self { … }
}
```

The output DTO `CommitPlan` (also `#[non_exhaustive]`) needs no change: its fields are READABLE
across the boundary (`#[non_exhaustive]` blocks struct-literal construction, not field reads), so
once `CommitInput` is constructible the PREVIEW assertions (`planned.new_commit`,
`planned.old_commit`, `planned.plan.nodes()`) and the COMMIT-through-`apply_shared` path are
reachable. The `write_*` tests for scenarios 3/8 are intentionally absent from the harness pending
this fix and are described in a banner at their position in `tests/e2e_planner.rs`.

## Concern + constructive proposal (per Critical Review Policy)

- **Concern (business traceability):** the driver's headline capability — "a git commit is `INSERT
  INTO /commits`" — is the single most important thing a stakeholder/AI must be able to DO with this
  driver, and today it is demonstrable only from inside the crate. The acceptance criteria's plan
  assertions for the write path cannot be honored by an external integration test as written.
- **Proposal:** ship the small `CommitInput::new(...).with_files(...)` builder above (a ~10-line,
  non-breaking, additive change). Once landed, I will add the three `write_*` tests
  (PREVIEW-applies-nothing, COMMIT-moves-branch+reflog, runtime-bridge-executes) and the
  plan-node-kind cross-check, and re-run; I expect 25–26/26 green and full N/N coverage. This keeps
  the strong git-as-effects-as-data story intact while making it reachable by the consumers that
  justify the driver's existence.

The round-1 concern (the headline "git commit is `INSERT INTO /commits`" capability was demonstrable
only from inside the crate) is now resolved: the public `CommitInput::new(...).with_file(...)`
builder landed in `74b7ad9` exactly as proposed, and the three `write_*` tests + the plan-node
cross-check are added and green (25/26 → 25-scenario harness, all green).

## Concern + constructive proposal (round 2 — per Critical Review Policy)

- **Concern (residual, low severity — write coverage breadth):** the closed write tests use a
  **flat single-file** commit (`with_file`), and the merge/rebase/commit builders all operate on a
  flat tree (the documented E0 "nested trees" park). So the external write surface is proven for the
  flat-tree case only; a commit/merge that touches a nested subtree (`src/main.rs`) has no E2E
  coverage because the feature itself is parked. This is a correctly-scoped E0 limitation, not a
  defect — but it means the driver's BlobFs read side (which resolves `src/lib.rs`-style paths in
  `GitPath`) is broader than its write side can build, an asymmetry a stakeholder should trace.
- **Proposal:** when the nested-tree write park is lifted (a later ticket), add a single E2E test
  that `INSERT`s a nested-path file and reads it back through `blobfs::cat(repo, ref, "dir/file")`,
  closing the read/write symmetry. No action needed for t26; recorded so the carry-over is explicit.

## Verdict

**9/9 scenarios PASS — t26 closes to N/N from the external interface.** 25 black-box E2E tests green;
full workspace 793 passed / 0 failed; clippy `--all-targets -D warnings` and `fmt --all --check`
both green. Approve with the one low-severity carry-over observation above (nested-tree write E2E,
deferred with the feature). STOP here per protocol — not advancing the workflow.
