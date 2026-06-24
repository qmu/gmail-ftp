# Architect Analytical Review — t40 (docs + distribution), commit `2aa01c0`

- **Author**: Architect (Neutral / structural bridge)
- **Ticket**: `.workaholic/tickets/todo/a-qmu-jp/20260622214650-t40-docs-and-distribution.md`
- **Scope**: analytical review only (no test/build/clippy execution — Lead owns the gate).
- **Decision**: **Approve with observations** (the final ticket of the trip).

---

## Headline 1 — The `qfs` `[lib]` facet + the two dep-guard relaxations

**Ruling: this is a principled, narrow, fail-closed exemption — NOT a weakening of the
confinement the trip built.** It is the structurally correct continuation of the t28→t39
"binary is the terminal-sink composition root" argument, extended to one new build-only leaf.
I walked all four sub-questions.

### (a) Is `xtask` genuinely a build-only terminal leaf, so the exemption opens no runtime hole? — YES.

`xtask/Cargo.toml` is `publish = false`, declares a single `[[bin]]`, and its ONLY dependency is
the `qfs` path crate (`xtask/src/main.rs` doc-comment: "the only dependency is the `qfs` path
crate … everything else is std"). Cargo metadata confirms nothing depends on `xtask`. tokio's
reach is unchanged: it can transit `xtask → qfs(lib) → qfs-driver-local → qfs-runtime`, but
`xtask` is itself a leaf, so it **still dead-ends** exactly as the binary does. The runtime
confinement test (`runtime_is_confined_to_plan_and_types`) was extended with a fourth
`other != "xtask"` exemption that is sound for the *same* "tokio dead-ends here" reason already
granted to `qfs` (t28) and `qfs-skill` (t39). The pattern is consistent, not novel.

### (b) Does "xtask is the SOLE permitted dependent of qfs" fail closed? — YES, mechanically.

`nothing_depends_on_the_qfs_binary_so_it_is_a_terminal_sink` does two things now:
1. The loop skips only `qfs` and `xtask`; any *other* package gaining a `→ qfs` edge fires the
   terminal-sink assertion.
2. A NEW, stronger `assert_eq!(qfs_dependents, vec!["xtask"])` pins the dependent set to exactly
   `["xtask"]`. A *second* dependent (even another leaf) trips this immediately — the comment is
   explicit that "a new dependent must be a conscious decision, re-reviewed against the soundness
   argument." This is genuinely fail-closed: the guard converts "xtask is the sole dependent" from
   prose into an enforced invariant, mirroring how t28 converted "the binary is a sink" into one.

A non-xtask edge into `qfs`'s lib also trips `runtime_is_confined_to_plan_and_types` (only `qfs`/
`qfs-skill`/`xtask` are exempt from the leaf check) — so there is defense in depth across two
independent tests.

### (c) Is routing keyword/codec constants through `qfs-core` keeping the spine acyclic and the binary off the lower spine? — YES.

`crates/core/src/lib.rs` re-exports `qfs_lang::{grammar_ebnf, RESERVED_KEYWORDS}` and
`qfs_codec::builtin_codecs`. The comment correctly notes `qfs-core` *already* depends on both
`qfs-lang` and `qfs-codec`, so this adds **no new edge** — it is a pure re-export. `qfs::docs`
imports them from `qfs_core` (`use qfs_core::{grammar_ebnf, Capabilities, RESERVED_KEYWORDS}`),
and `qfs::catalog` reads `qfs_core::builtin_codecs`. The binary's lib therefore takes NO direct
`qfs-lang` / `qfs-codec` edge — it stays off the lower spine, exactly as `qfs-cmd` does. The
existing `binary_is_the_thin_entrypoint_plus_the_t28_shell_composition_root` allowlist is
unchanged (no new lower-spine member appears in the binary's deps), which confirms the routing
held. The spine stays acyclic: `qfs(lib) → qfs-core → {qfs-lang, qfs-codec}`, one-directional.

### (d) Was giving the binary a `[lib]` facet the right call vs. alternatives? — YES, and the rejected alternatives are weaker.

The doc generator *must* read the binary's OWN describe registry (`describe_registry()` lives in
`crates/qfs/src/describe.rs` — the composition root that wires the eight cred-free drivers). The
anti-drift guarantee is precisely "docs derive from the registry the binary ships." Three
alternatives, all worse:
- **xtask shells out to `qfs describe`** — would re-derive docs from CLI *text* output, not the
  registry; brittle, and it could not reach `grammar_ebnf` / `builtin_codecs` without more
  plumbing. Worse fidelity.
- **doc generator in a small shared crate** — that crate would itself have to depend on every
  driver crate to build the registry, recreating `describe.rs`. It would become a *second*
  composition root (the very duplication the trip's single-sink topology avoids), and it would be
  a non-leaf reaching the driver crates — a new spine wrinkle. Strictly worse than reusing the one
  existing composition root.
- **`Driver::doc()` trait method (the ticket's literal sketch)** — correctly REJECTED (ADR-0007
  §2, `catalog.rs` header): a new *required* trait method forces every driver crate + downstream
  to recompile, which the constrained disk cannot survive. Reusing the t39 `DescribeReport` avoids
  even a default-method change. This is the better engineering call AND the better structural call.

The `[lib]` facet is the minimal surface that lets the ONE composition root be reused by two
consumers (`main.rs` + `xtask`) without duplicating it. **Not a blocker, not a carry-over** — I
would not prefer a different topology.

**One observation (O1, structural-honesty):** the lib facet exposes ALL of the binary's modules
as `pub mod` (`serve`, `shell`, `host`, `cron`, `watchtower`, `serve_builtins`), not just the
pure `catalog`/`docs`/`version` surface xtask needs. The `lib.rs` header is honest that these are
"`pub(crate)`-only in spirit … nothing outside this crate links the binary except xtask, which
touches only the pure catalog/docs/version surface." That honesty is good, but "in spirit" is not
enforced: a future second consumer of the lib could reach the runtime-coupled modules. **Proposal:**
the `assert_eq!(qfs_dependents, vec!["xtask"])` guard already closes this (no second consumer can
appear un-reviewed), so the surface is *contained* even though it is *wide*. I accept it as-is for
t40; a carry-over to gate `serve`/`shell`/`host`/etc. behind `pub(crate)` + a thin
`pub use`-only doc-surface module (so the lib literally cannot expose the runtime modules) would be
a clean follow-up, not a t40 blocker.

---

## Headline 2 — Anti-drift docs-as-derived

**Confirmed on all four points.**

- **Docs derive from the binary's own registries.** `render_drivers()` calls
  `catalog::driver_catalog()`, which walks `describe_registry()` (the binary's live registry) and
  folds each driver's t39 `DescribeReport` (archetype / verbs / procedures / aliases / pushdown)
  into the owned `DriverDoc` DTO. `render_language()` reads `qfs_core::RESERVED_KEYWORDS` +
  `grammar_ebnf()`. Codecs come from `qfs_core::builtin_codecs()`. There is no hand-authored data
  path — the renderers are pure `-> String` functions of the registries. Verified the field
  accessors all exist (`DescribeReport.native_verbs`/`archetype`/`verbs`/`procedures`/`aliases`/
  `pushdown`; `AliasFn.name`/`desugars_to`; `ProcSig.name`/`params`/`irreversible`).
- **The golden test is non-vacuous.** `committed_docs_match_generated_output` computes the repo
  root from `CARGO_MANIFEST_DIR`'s grandparent, calls `check_docs(repo_root)` which reads each
  committed `docs/*.md` and byte-compares it to the freshly rendered string, asserting `drift`
  is empty. A missing committed file reads as `unwrap_or_default()` = `""` ≠ content → drift, so
  deletion is also caught. I spot-checked the committed `docs/drivers.md` against the renderer:
  the generated banner, the mount headers, the `/mail` ✓/✗ verb table, the `send` irreversible
  proc row, and the `SEND → mail.send` alias line all match what `render_driver()` emits. The
  test is real, not vacuous.
- **Unsupported verbs render explicitly (RFD §5).** `capability_rows()` returns a fixed 9-tuple
  (SELECT/INSERT/UPSERT/UPDATE/REMOVE/LS/CP/MV/RM); `render_driver()` emits a `✓`/`✗` row for
  EVERY verb, supported or not. `drivers_doc_shows_unsupported_verbs_explicitly` asserts the doc
  contains `✗`. Confirmed in the committed doc: `/mail` shows `UPDATE | ✗`, `LS | ✗`, etc. — never
  by omission. (Note: `/drive`'s representative node `/drive/Reports` shows ALL nine verbs `✗` —
  see O2 below; this is the genuine describe output for that node, faithfully rendered, not a
  generator bug.)
- **describe-reuse faithfully covers the catalog, and the t39 gap is honestly noted.**
  `driver_catalog()` folds the 8 describe-registered mounts (local/mail/drive/github/slack/ga/s3/
  r2). The sql/git/cf-d1 drivers are NOT describe-registered (they need a registered connection-
  catalog / repo / D1-catalog — `describe.rs` documents this as the CO-t29-1 light-facet gap).
  They are therefore honestly ABSENT from the catalog rather than silently faked. The README's
  hero line still lists "SQL, git" as supported services (they ARE supported via the skill golden
  corpus), and `docs/drivers.md` honestly catalogs only the describe-registered eight. This is a
  faithful translation of the t39 coverage state, not a drop.

**Observation (O2, fidelity):** the `/drive` catalog row shows every universal verb as `✗`
because the GDrive describe for `/drive/Reports` returns an empty capability set at that node.
Structurally this is correct (capabilities are path-keyed; the *folder* node may legitimately
support nothing the catalog's representative path exercises), and rendering it explicitly is the
RFD §5 intent. But an agent reading the catalog sees "/drive supports no verbs," which under-sells
the driver. **Proposal:** pick a representative `/drive` node that exercises the driver's real
capabilities (e.g. a file node rather than the `Reports` folder), OR add a one-line note in the
generated row that the representative node is a folder. Carry-over (a `representative_path` tweak),
not a t40 blocker — the golden is faithful to the chosen node either way.

---

## Headline 3 — Frozen-keyword governance (RFD §3)

**Confirmed: genuinely single-sourced.** `RESERVED_KEYWORDS` in `crates/lang/src/reference.rs` is
`pub const RESERVED_KEYWORDS: &[&str] = KEYWORDS;` — a direct alias of the one committed
`crate::keywords::KEYWORDS` slice, NOT a re-transcription. `reserved_keywords_is_the_frozen_set`
asserts `std::ptr::eq(RESERVED_KEYWORDS, KEYWORDS)` (pointer identity — the strongest possible
single-source proof), plus `.len() == KEYWORDS.len()`, equality, and the `== 38` freeze count.
`grammar_uses_only_frozen_vocabulary` asserts every frozen keyword AND operator appears as a quoted
terminal in `grammar_ebnf()`, so the grammar cannot reference vocabulary outside the frozen set.
The mechanical chain holds: adding a keyword to `keywords.rs` flows through the alias into
`render_language()`'s rendered table, which makes the committed `docs/language.md` stale, which
fails `committed_docs_match_generated_output` (and `xtask gen-docs --check` in CI). **The §3
"keyword set is frozen" promise is mechanically enforced, not documented-only.**

---

## Other surfaces

1. **Purity doctest — confirmed real.** `GmailDriver::send_alias_plan` (`crates/driver-gmail/src/
   lib.rs`) has a `///` doctest that calls `send_alias_plan("id:draft-1")`, asserts the returned
   `Plan` has exactly one node, that node is `EffectKind::Call(proc)` with `proc.0 == "mail.send"`,
   `irreversible`, and `plan.is_irreversible()`. The implementation builds the plan purely
   (`Target::new` + `EffectNode::new(...).irreversible(true)` + `Plan::leaf`) — no applier, no
   credential, no socket. All API accessors verified to exist (`Plan::leaf`/`nodes`/
   `is_irreversible`, `EffectNode.kind`/`irreversible`, `EffectKind::Call`, `ProcId(pub String)`).
   This is a genuine compile-and-run witness of desugar-without-I/O.

2. **`qfs --version` long form — sound, no spine leak.** `version.rs` reads `CARGO_PKG_VERSION` +
   the three `build.rs` `rustc-env` values (`QFS_GIT_SHA` best-effort `unknown` off-git,
   `QFS_TARGET`, `QFS_WASM_CAPABLE` derived from `target.starts_with("wasm32")`). `main.rs`
   intercepts a *standalone* `--version`/`-V` (`rest.len() == 1`) BEFORE `qfs_cmd::run`, so
   qfs-cmd stays off the build-metadata surface and the flag never shadows a subcommand argument.
   `long_version_carries_semver_sha_and_target` asserts the shape and that no `token`/`Bearer`
   leaks. `build.rs` embeds no secret. Sound.

3. **Release/dist/wasm CI-only scoping — honestly parked, not overclaimed.** `cmd_dist` refuses to
   run without `QFS_DIST_ALLOW=1`, prints the real matrix it WOULD run (`print_dist_plan`), and
   produces no faked artifact when refused. `build_wasm` builds `-p qfs-host` (the wasm-clean
   facet, per t36/ADR-0005) — NOT the full binary — and copies `qfs_host.wasm` only `if
   artifact.exists()`, which is honest about the full-binary wasm artifact still being parked.
   ADR-0007 §4 + the "Negative / parked" consequence state plainly that the four tarballs + wasm
   are "not produced or verified on the trip host … asserted by reviewable code + `release.yml`."
   The limits are framed as environmental (no cross-linker, not wasm-clean), consistent with
   t36/ADR-0005. README's "Offline / disk scoping" callout matches. No overclaim of "ships today."

4. **Docs accuracy + no-creds — confirmed.** README is the authoritative spec (vision / core-model
   / install / quickstart / SemVer=grammar-stable / links to `qfs skill`); all in-README doc links
   resolve (`docs/language.md`, `docs/drivers.md`, `docs/server.md`, `docs/README.md`,
   `docs/adr/0005-deployment-hosts.md`, `crates/skill/assets/SKILL.md` all exist). The generated
   docs match the registries (verified above). `install.sh` fetches the tarball + `.sha256`,
   computes the local hash, and `die`s on mismatch BEFORE `tar -xzf` — verify-before-extract is
   correct. The credential gate (`scripts/check-no-live-credentials.sh`) greps README/docs/install
   .sh/goldens/dist for live-token VALUE shapes (ghp_/xox*/AKIA/ya29./JWT/PEM/sk-) and fails on a
   hit; docs use placeholder handles only. `generated_docs_carry_no_live_credentials` and
   `catalog_leaks_no_credential_shape` back this in-process. (Note: I did not EXECUTE the grep gate
   — analytical-only — but the scan paths + patterns are correct and the docs I read carry only
   placeholders.)

---

## Cross-cutting concern (single observation, per Critical Review Policy)

**O3 — the dep-direction guard file now carries the WHOLE trip's confinement reasoning in one
test file, and t40 added a third exemption (xtask) to two of its tests.** This is structurally the
right place (the guard is the mechanical record of the topology), and each exemption is justified
by the same proven invariant. The risk is purely *legibility*: a future reader must hold the t28/
t39/t40 "tokio dead-ends in a leaf" argument across `nothing_depends_on_the_qfs_binary…`,
`runtime_is_confined_to_plan_and_types`, and the per-binding leaf tests to see that the three
exempt nodes (`qfs`, `qfs-skill`, `xtask`) share ONE rationale. **Proposal (carry-over, not a
blocker):** extract a single named constant `TERMINAL_LEAVES = ["qfs", "qfs-skill", "xtask"]` with
the shared "tokio dead-ends here" doc-comment, and reference it from each exemption, so the three
sites cannot drift apart and the rationale lives in exactly one place. t40 already keeps them
correct; this only makes the *why* single-sourced the way the keyword set now is.

---

## Decision

**Approve with observations.** The load-bearing structural decision — the `qfs` `[lib]` facet +
the xtask-only guard relaxations — is a **principled, narrow, fail-closed exemption**, not a
weakening: xtask is a build-only terminal leaf (tokio still dead-ends), the sole-dependent
invariant is mechanically pinned (`assert_eq!(qfs_dependents, vec!["xtask"])`), the keyword/codec
constants route through `qfs-core` with zero new spine edge, and the lib facet is the minimal reuse
of the one composition root (strictly better than the three rejected alternatives). Anti-drift
docs-as-derived, the ptr-identity frozen-keyword single-sourcing, the purity doctest, the
version-metadata interception, and the honest CI-only artifact parking all hold. Observations
O1/O2/O3 are carry-overs (lib-surface narrowing, the `/drive` representative-node choice, and a
`TERMINAL_LEAVES` constant), none of which block t40. Cleared for Planner E2E.
