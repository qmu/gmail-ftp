# Coding E2E Review (Planner) — t03 Lexer + t04 Grammar/AST

Author: Planner (Progressive)
Phase: Coding — E2E / external interface testing only (no code review)
Targets: t03 (`qfs-lang::lex`) + t04 (`qfs-parser::parse_statement`)
Method: throwaway external consumer crate in `/tmp/qfs-e2e-consumer` (own
`[workspace]` table; path-dep on `crates/parser`), calls **only** the public
surface `qfs_parser::parse_statement(&str) -> Result<Statement, ParseError>` and
serializes the `Statement` AST to JSON via `serde_json`. No production code added;
the `/tmp` crate is removed after this run. 56/56 checks passed.

## How the front door was exercised

`parse_statement` is the single public entry. It lexes via the t03 tokenizer
(`qfs_lang::lex`) and parses the full t04 grammar; t03 is therefore validated
*transitively* through the same front door (size literals `25 MB`, typed literals
`DATE '…'`, paths `/git/repo@v1.2/src`, the `=>` named-arg arrow, the `|>` pipe,
operators `~ <> <= >=`, strings/ints/floats/bools all appear in the AST dumps
below, proving the lexer produced the right tokens).

---

## 1. VALID acceptance-criteria forms — all PASS

| Item (t04 AC) | Input (representative) | Result |
| --- | --- | --- |
| Query `FROM…|>WHERE…|>SELECT…` | `FROM /mail/inbox |> WHERE size > 25 MB AND subject ~ 'invoice' |> SELECT id, subject AS title` | PASS — `Ok(Statement::Query)` |
| Full multi-op chain | `FROM…|>WHERE|>SELECT|>JOIN…ON…|>AGGREGATE count(id) AS n|>GROUP BY|>ORDER BY n DESC|>LIMIT 10` | PASS |
| EXPAND | `FROM /mail/inbox |> EXPAND attachments` | PASS |
| DISTINCT | `FROM /t |> DISTINCT` | PASS |
| UNION / EXCEPT / INTERSECT | `FROM /a |> UNION FROM /b` (+ EXCEPT, INTERSECT) | PASS (×3) |
| `@version` path | `FROM /git/repo@v1.2/src |> SELECT path` | PASS |
| `AS OF` temporal | `FROM /sql/pg/orders AS OF '2026-01-01'` | PASS |
| glob + `@version` | `FROM /s3/bucket/*.json@latest` | PASS |
| struct nav `a.b.c` | `FROM /t |> WHERE a.b.c = 1` | PASS |
| IN / BETWEEN / ANY | `… WHERE id IN (1,2,3) AND price BETWEEN 10 AND 20 AND x = ANY (4,5)` | PASS |
| typed-lit BETWEEN | `… WHERE created BETWEEN DATE '2026-01-01' AND DATE '2026-06-20'` | PASS |
| function registry `fn(…)` | `FROM /t |> SELECT upper(name)` | PASS |
| INSERT … RETURNING | `INSERT INTO /mail/drafts VALUES (to, subject) ('a@b.c','hi') RETURNING id` | PASS — `verb: Insert` |
| UPSERT | `UPSERT INTO /s3/bucket/key VALUES ('blob')` | PASS — `verb: Upsert` (distinct) |
| UPDATE … SET … WHERE | `UPDATE /sql/pg/orders SET status = 'done' WHERE id = 7` | PASS |
| REMOVE … WHERE | `REMOVE /mail/spam WHERE age > 30` | PASS |
| INSERT … FROM sub-pipeline | `INSERT INTO /archive FROM /mail/inbox |> WHERE flag = TRUE` | PASS |
| DECODE / ENCODE | `FROM /fs/data.json |> DECODE json |> ENCODE yaml` | PASS |
| CALL driver.action(...) | `… |> CALL mail.send |> CALL github.merge(method => 'squash', 42)` | PASS — named + positional args |
| CREATE ENDPOINT … ON … AS … | `CREATE ENDPOINT recent ON 'GET /recent' AS FROM /mail/inbox |> LIMIT 5` | PASS |
| CREATE TRIGGER … ON … DO … | `CREATE TRIGGER notify ON inbox DO INSERT INTO /log VALUES ('fired')` | PASS |
| CREATE JOB … EVERY … DO … | `CREATE JOB nightly EVERY '1h' DO REMOVE /tmp WHERE age > 7` | PASS |
| CREATE VIEW | `CREATE VIEW recent AS FROM /mail/inbox` | PASS |
| CREATE MATERIALIZED VIEW | `CREATE MATERIALIZED VIEW cached AS FROM /mail/inbox` | PASS |
| CREATE WEBHOOK | `CREATE WEBHOOK inbound ON '/hooks/x'` | PASS |
| CREATE POLICY | `CREATE POLICY leastpriv` | PASS |
| PREVIEW wrapper | `PREVIEW REMOVE /mail/spam WHERE age > 30` | PASS — `commit: false` |
| COMMIT wrapper | `COMMIT INSERT INTO /t VALUES (1)` | PASS — `commit: true` |

Every valid input returned `Ok(Statement)` and serialized cleanly to JSON. All
seven `DdlKind` forms, all four `EffectVerb`s, both plan wrappers, and the
codec/call/fn registry seams are reachable from the outside.

### Sample valid-AST JSON dumps

**Query** — `FROM /mail/inbox |> WHERE size > 25 MB AND subject ~ 'invoice' |> SELECT id, subject AS title`
(abridged; shows t03 size-literal `25 MB`, the `~` match op, and `AS` alias all
landing in the t04 AST with byte spans):

```json
{
  "Query": {
    "source": { "Path": { "segments": [
      { "name": "mail", "version": null, "glob": false },
      { "name": "inbox", "version": null, "glob": false }
    ], "as_of": null, "span": [5, 16] } },
    "ops": [
      { "Where": { "Binary": { "op": "And",
        "lhs": { "Binary": { "op": "Gt",
          "lhs": { "Col": "size" },
          "rhs": { "Lit": { "Size": { "value": 25, "unit": "MB" } } } } },
        "rhs": { "Binary": { "op": "Match",
          "lhs": { "Col": "subject" },
          "rhs": { "Lit": { "Str": "invoice" } } } } } } },
      { "Select": [
        { "Expr": { "expr": { "Col": "id" }, "alias": null } },
        { "Expr": { "expr": { "Col": "subject" }, "alias": "title" } } ] }
    ]
  }
}
```

**Effect** — `INSERT INTO /mail/drafts VALUES (to, subject) ('a@b.c', 'hi') RETURNING id`:

```json
{
  "Effect": {
    "verb": "Insert",
    "target": { "segments": [
      { "name": "mail", "version": null, "glob": false },
      { "name": "drafts", "version": null, "glob": false }
    ], "as_of": null, "span": [12, 24] },
    "body": { "Values": {
      "columns": ["to", "subject"],
      "rows": [[ { "Lit": { "Str": "a@b.c" } }, { "Lit": { "Str": "hi" } } ]] } },
    "returning": [ { "Expr": { "expr": { "Col": "id" }, "alias": null } } ]
  }
}
```

**CALL with named + positional args** — `… |> CALL github.merge(method => 'squash', 42)`
(confirms the t03 `=>` arrow drives the t04 named-arg parser):

```json
{ "Call": {
  "driver": "github", "action": "merge",
  "args": [
    { "name": "method", "value": { "Lit": { "Str": "squash" } } },
    { "name": null,     "value": { "Lit": { "Int": 42 } } }
  ],
  "span": [39, 56] } }
```

---

## 2. Governance / structured-error cases — all PASS

Every adversarial/governance input returned `Err(ParseError)`. The consumer
confirmed it can branch on `code`, every error has a **non-empty** `expected` set,
and every `span` is within source bounds.

| Item | Input | `code` | branchable? | expected non-empty? |
| --- | --- | --- | --- | --- |
| lowercase keyword | `FROM /mail |> where id = 1` | `UnknownKeyword` | yes | yes |
| reserved-as-identifier | `FROM /t |> SELECT SELECT` | `ReservedAsIdentifier` | yes | yes |
| dangling pipe op (WHERE) | `FROM /mail |> WHERE` | `UnexpectedEof` | yes | yes |
| unknown construct | `FROM /mail |> BANANA` | `UnexpectedToken` | yes | yes |
| missing pipe | `FROM /mail WHERE id = 1` | `ReservedAsIdentifier` | yes | yes |
| empty input | `""` | `UnexpectedEof` | yes | yes |
| unterminated string | `FROM /t |> WHERE x = 'oops` | `UnexpectedToken` (msg `lexing failed: UNTERMINATED_STRING`) | yes | yes |
| dangling pipe operator | `FROM /t |>` | `UnexpectedEof` | yes | yes |

Two full error dumps:

```
ERR [err/lowercase-keyword]  src: "FROM /mail |> where id = 1"
ParseError {
  at: 14, span: [14, 19],
  code: UnknownKeyword (UNKNOWN_KEYWORD),
  expected: ["FROM","INSERT INTO","CREATE","PREVIEW","COMMIT","|>","a path"],
  found: "an identifier",
  message: "closed-core keywords are UPPERCASE (RFD §3)"
}
Display: [UNKNOWN_KEYWORD] at byte 14 | expected: FROM, INSERT INTO, CREATE, PREVIEW, COMMIT, |>, a path | found: an identifier | closed-core keywords are UPPERCASE (RFD §3)
```

```
ERR [err/unterminated-string]  src: "FROM /t |> WHERE x = 'oops"
ParseError {
  at: 21, span: [21, 26],
  code: UnexpectedToken (UNEXPECTED_TOKEN),
  expected: ["a valid token"],
  found: "an unlexable character",
  message: "lexing failed: UNTERMINATED_STRING"
}
Display: [UNEXPECTED_TOKEN] at byte 21 | expected: a valid token | found: an unlexable character | lexing failed: UNTERMINATED_STRING
```

Verified `ParseError` fields (all owned, no winnow type leaks; `Clone`/`Eq`/`Display`):
- `at: usize` — byte offset.
- `span: Span { start, end }` — `span.start == at`; round-trips inside source.
- `code: ParseErrorCode` — `{UnexpectedToken, UnexpectedEof, UnknownKeyword, ReservedAsIdentifier}`, `#[non_exhaustive]`, branchable, `.as_str()` machine code.
- `expected: Vec<String>` — always non-empty (RFD §5 contract holds).
- `found: String` — token **kind** only (e.g. `"an identifier"`, `"keyword \`SELECT\`"`), never a literal value.
- `message: String` — human-facing.

**Concern (minor, advisory — does NOT block):** the t03 lexer error
`UnterminatedString` is remapped at the parser boundary to the generic parser code
`UnexpectedToken` with `expected: ["a valid token"]`; the original lexer kind
survives only inside the `message` string (`"lexing failed: UNTERMINATED_STRING"`).
A consumer that wants to branch *specifically* on "lexer vs grammar" failure cannot
do so on `code` alone today. **Proposal:** when E2 surfaces fine-grained recovery,
add a `LexError`/`UnterminatedString` discriminant to `ParseErrorCode` (it is
`#[non_exhaustive]`, so this is a non-breaking, change-controlled addition). For E0
this is acceptable: the structured-error contract (span + non-empty expected +
machine code + no panic) is fully satisfied.

---

## 3. Secret hygiene (RFD §10) — PASS

| Item | Input | literal that must NOT appear | result |
| --- | --- | --- | --- |
| WHERE literal | `FROM /mail |> WHERE secret = 'p@ssw0rd' BANANA` | `p@ssw0rd` | PASS — not echoed |
| INSERT literal | `INSERT INTO /vault VALUES ('AKIA-SUPER-SECRET-KEY') BANANA` | `AKIA-SUPER-SECRET-KEY` | PASS — not echoed |

For both, the error `Display` describes the offending token by **kind**
(`found: "an identifier"`, pointing at the trailing `BANANA`) and never quotes the
credential literal. The redaction claim in the t04 ticket holds for the error
display path.

---

## 4. Adversarial / no-panic robustness — all PASS

`std::panic::catch_unwind` around `parse_statement`; every input returned (Ok or
Err) with **no panic / no abort / no backtrace**:

- very long input (~50 KB, 5000 `AND` conjuncts) — SAFE
- deep nesting (2000 `NOT` prefixes) — SAFE
- unicode (`FROM /メール/受信箱 |> WHERE 件名 ~ '請求書' AND 🔥 = 1`) — SAFE
- whitespace-only — SAFE
- embedded newlines / `\r\n` — SAFE
- embedded NUL (`x\0= 1`) — SAFE
- lone `|>` — SAFE
- only slashes `////////` — SAFE
- control chars / ANSI escape bytes — SAFE
- unbalanced parens `((((((` — SAFE
- huge numeric literal (30 digits, overflows i64) — SAFE (no panic on overflow)
- UTF-8 BOM prefix — SAFE

This satisfies the t03 property-test promise ("`lex` never panics on arbitrary
input") at the integrated `parse_statement` level.

---

## Item-by-item PASS/FAIL

1. Valid acceptance-criteria forms (query, effect+RETURNING, DECODE/ENCODE, CALL, all CREATE DDL, PREVIEW/COMMIT) — **PASS** (Ok + JSON dumped for each)
2. Governance/error cases (lowercase, reserved-as-ident, dangling pipe, unterminated string, unknown construct, empty) — **PASS** (Err with span + machine code + non-empty expected; consumer branches on `code`)
3. No panic on adversarial inputs (long, unicode, whitespace, newlines, NUL, …) — **PASS**
4. Secret hygiene (no literal echoed in error Display) — **PASS**

## Verdict

**E2E approved.** The t03 lexer + t04 grammar front door (`parse_statement`)
behaves correctly from the outside: all acceptance-criteria statement forms parse
to the expected owned `Statement` AST and serialize to JSON; all governance/error
cases yield a structured, owned `ParseError` (byte span + machine code + non-empty
expected-set) that a consumer can branch on; no input panics; and the error display
does not leak literal values. One non-blocking advisory: lexer-origin failures are
folded into the `UnexpectedToken` code (detail preserved only in `message`) — worth
a `#[non_exhaustive]` code addition in E2, not a blocker for E0.
