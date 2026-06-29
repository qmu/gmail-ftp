# Where qfs is going next

A forward-looking design note (設計書), not a description of shipped behavior. Each item is tagged
**✅ shipped** (true today — see the guides) or **🧭 proposed** (a plan, not yet runnable). The
[guide](/guide/concepts) and [cookbook](/cookbook/) only ever document ✅; this page is the one place
that talks about 🧭. (The previous roadmap was erased once it all shipped as v0.0.9; this is the next
one.)

## What just shipped ✅ — the "make the docs true" cycle

The binary was wired so the headline capabilities run for real, instead of erroring for a fresh user:

- **`/local` file content + codecs** — `/local/<file>.json |> decode json |> encode yaml` actually
  transcodes (was a silent no-op).
- **`/sql` reads** (SQLite) with the `WHERE` pushed **into** the database; **`/git` reads** (commits,
  refs, tags, reflog, HEAD-tree listings) over the local object store.
- **Cloud reads fail honestly** — `/mail`, `/drive`, `/github`, … return an actionable *"connect a …
  account"* capability error instead of `unknown_source`; **gmail** returns real messages once
  connected.
- **No WARN noise** on unrelated runs; **`describe`** verb maps now derive from real capabilities.

Every doc page was rewritten and verified example-by-example against the binary.

## The next change 🧭 — connections become a language *declaration*

### The problem with today's model

A connection is **injected from outside the language by a naming convention**. `/sql/orders` works
only because the binary scans `QFS_SQL_ORDERS=<path>` and lower-cases the suffix into the path
segment; `/git/<repo>` is the same. There is no declaration you can read, review, or version — the
source of truth is an *environment variable's name*. Credentialed services use a *different* model
again (an encrypted store + an "active connection" selector). Two incoherent mechanisms, neither
declared in the language.

### The design

A connection should be an **explicit statement in qfs**, consistent with the `CREATE TRIGGER` /
`CREATE POLICY` it already has — reviewable, versionable, and committable as config. Secrets are
**referenced**, never inlined; the alias is never loaded by matching an env-var prefix.

```text
🧭 proposed — declare the alias; reference the secret; the name is the path segment

CREATE CONNECTION orders
  DRIVER sqlite
  AT '/data/orders.db'                    -- → read at /sql/orders/<table>

CREATE CONNECTION analytics
  DRIVER postgres
  AT 'postgres://db.internal/analytics'
  SECRET env:PG_PASSWORD                  -- value from $PG_PASSWORD, never written in the file

CREATE CONNECTION app
  DRIVER git
  AT '/srv/repos/app.git'                 -- → /git/app/commits, /git/app@<ref>/…

CREATE CONNECTION work
  DRIVER gmail
  SECRET vault:gmail/work                 -- token from the encrypted store → /mail/work/…
```

- **`DRIVER`** picks the path family (`sqlite`/`postgres`/`mysql` → `/sql`, `git` → `/git`, `gmail`
  → `/mail`, …); the connection **name** is the segment you write in the path.
- **`AT '<locator>'`** is the non-secret location (file / URI / bucket); optional when the driver's
  locator is implicit.
- **`SECRET env:<VAR>` or `SECRET vault:<path>`** — resolved at use time, never logged, never in a
  `describe`. A local SQLite/git connection needs no secret. The vault is the *same* encrypted store
  `qfs connection add` writes — so `add` provisions the secret a `vault:` reference resolves.
- Declarations live in a **`connections.qfs`** config, loaded by `qfs serve` / `qfs job` and by
  `qfs run --config` (plus a default config path). Explicit config you commit to a repo — not env
  scanning.

This **replaces** `QFS_SQL_<conn>=<path>` (implicit) with `CREATE CONNECTION orders DRIVER sqlite AT
'/data/orders.db'` (explicit). The env vars become a deprecated shim with a one-line migration
(`qfs connection import-env` prints the equivalent declarations).

### The plan (this cycle's tickets)

Tracked as EPIC `20260630004100` + 8 phase tickets in the backlog:

| Phase | What | Status |
| --- | --- | --- |
| **Design (keystone)** | finalize the grammar, the driver→path-family map, the `/sql`-name-in-path vs `/mail` active-connection reconciliation, the secret-ref scheme, and the env-var migration | 🧭 |
| **Parser** | `CREATE CONNECTION` keywords + AST + grammar + a parse-ratchet recipe | 🧭 |
| **Secret resolution** | `env:` / `vault:` resolvers over the encrypted store; lazy, secret-free | 🧭 |
| **Registry from declarations** | build the mount + read/apply registries from declarations, replacing the env scan in sql/git/google/objstore | 🧭 |
| **Config + runtime loading** | `.qfs` config carries declarations; `serve` / `job` / `run --config` load them | 🧭 |
| **Deprecate env convention** | `QFS_SQL_*`/`QFS_GIT_*` → a warned shim + `connection import-env` | 🧭 |
| **CLI alignment** | `connection add` stores the secret a `vault:` ref resolves; `list` shows the declared set | 🧭 |
| **Docs** | flip the guide/cookbook to the declaration model (done last, so docs stay honest) | 🧭 |

The keystone is the **design** ticket — the grammar is part of the [versioned surface](/), so it is
finalized deliberately before any code.

## Near-term backlog 🧭 — follow-ups from the wiring cycle

Real gaps the doc-honesty pass surfaced; the guides already avoid claiming these run:

- **`/drive` and `/ga` real reads** — `/drive` needs path→folder-id resolution; `/ga` needs a
  query→`runReport` (dimensions/metrics) mapping. Both stay on the connect-account error until then.
- **`/git@<ref>` temporal reads** — `commits`/`refs`/`tags`/`reflog` and the HEAD tree run; the
  `@<ref>` coordinate is **not yet honored** for tree/blob reads, and reading a *single file's bytes*
  at a ref errors. Ref-pinned trees + file content are a follow-up.
- **`/local` write materialization** — a `upsert into /local/<file> …` previews and reports
  committed, but does not yet write the file (`carries no content blob`).
- **`md` codec** — `decode/encode md` errors `unknown_codec`; only `json/jsonl/yaml/toml/csv` are
  registered, though the markdown+frontmatter codec exists in the codec crate.
- **`/cf` and `/rest` mounts** — the driver crates exist but are not mounted in the CLI.
- **Pushdown depth** — gmail `q=` `WHERE` pushdown; `/sql` projection/`ORDER`/`LIMIT` and `/git`
  ref-range/`LIMIT` pushdown (today only `WHERE`/the ref is native, the rest is the engine residual).
- **Plain-language onboarding** — the first-run and credential docs are too jargon-heavy to land
  with a normal user (e.g. `install.sh` and `connections.md` explain `QFS_PASSPHRASE` as "the master
  passphrase that unlocks your local credential vault (argon2id KDF; NOT a service credential)").
  Rewrite the first-encounter wording in plain terms — *a password you choose that encrypts the
  service logins you save on this machine* — and keep the crypto detail (argon2id, envelope
  encryption) only in the dedicated "how it's stored" / threat-model sections for readers who want it.

These are intentionally honest gaps, not regressions — the binary fails closed or returns a clear
error for each, and no guide example depends on them.
