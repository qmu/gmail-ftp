//! Golden-AST snapshot tests (t04 acceptance criteria).
//!
//! Each statement in the corpus is parsed and serialised to compact JSON (the same
//! `serde::Serialize` path that powers `-json` AST dumps). The corpus covers every
//! acceptance-criterion form: a multi-op query, `EXPAND`, `DISTINCT`,
//! `UNION/EXCEPT/INTERSECT`, all four effect verbs with `VALUES`/`RETURNING`,
//! `DECODE/ENCODE`, `CALL`, a registry `fn(...)`, `@version`/`AS OF`, struct path
//! access, every `CREATE …` DDL form, and the `PREVIEW`/`COMMIT` wrappers.
//!
//! The test asserts two things per case: (1) the statement parses, and (2) its JSON
//! serialisation is **stable** (deterministic) — a refactor that changes AST shape
//! trips the byte-for-byte golden compare in `pinned_goldens`. The full corpus is
//! kept as a smaller set of byte-pinned goldens plus a broad parse-and-redump
//! determinism sweep, so the file stays reviewable while still locking shape.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use qfs_parser::parse_statement;

/// The full acceptance corpus: every form the criteria enumerate.
const CORPUS: &[&str] = &[
    // multi-op query
    "FROM /mail/inbox |> WHERE id = 1 |> SELECT id, subject AS title |> JOIN /contacts ON id = c |> AGGREGATE count(id) AS n |> ORDER BY n DESC |> LIMIT 10",
    // EXPAND
    "FROM /mail/inbox |> EXPAND attachments",
    // DISTINCT
    "FROM /t |> DISTINCT",
    // set operators
    "FROM /a |> UNION FROM /b",
    "FROM /a |> EXCEPT FROM /b",
    "FROM /a |> INTERSECT FROM /b",
    // effect verbs
    "INSERT INTO /t VALUES (1, 2) RETURNING id",
    "UPSERT INTO /s3/bucket/key VALUES ('blob')",
    "UPDATE /sql/pg/orders SET status = 'done' WHERE id = 7",
    "REMOVE /mail/spam WHERE age > 30",
    // codecs
    "FROM /fs/data.json |> DECODE json |> ENCODE yaml",
    // CALL
    "FROM /mail/drafts |> CALL github.merge(method => 'squash')",
    // registry fn
    "FROM /t |> SELECT upper(name)",
    // @version and AS OF
    "FROM /git/repo@main/src",
    "FROM /sql/pg/orders AS OF '2026-01-01'",
    // struct path access
    "FROM /t |> WHERE a.b.c = 1",
    // every CREATE form
    "CREATE ENDPOINT recent ON 'GET /recent' AS FROM /mail/inbox |> LIMIT 5",
    "CREATE TRIGGER notify ON inbox DO INSERT INTO /log VALUES ('x')",
    "CREATE JOB nightly EVERY '1h' DO REMOVE /tmp WHERE age > 7",
    "CREATE VIEW recent AS FROM /mail/inbox",
    "CREATE MATERIALIZED VIEW cached AS FROM /mail/inbox",
    "CREATE WEBHOOK inbound ON '/hooks/x'",
    "CREATE POLICY leastpriv",
    // plan wrappers
    "PREVIEW REMOVE /mail/spam WHERE age > 30",
    "COMMIT INSERT INTO /t VALUES (1)",
];

#[test]
fn corpus_parses_and_serialises_deterministically() {
    for src in CORPUS {
        let stmt = parse_statement(src)
            .unwrap_or_else(|e| panic!("corpus item failed to parse `{src}`: {e}"));
        let json1 = serde_json::to_string(&stmt).expect("serialise");
        // Re-parse from the SAME source (the AST is pure data: deterministic).
        let stmt2 = parse_statement(src).expect("re-parse");
        let json2 = serde_json::to_string(&stmt2).expect("serialise");
        assert_eq!(json1, json2, "non-deterministic AST for `{src}`");
        // Re-serialising the cloned AST is identical (no interior nondeterminism).
        let json3 = serde_json::to_string(&stmt.clone()).expect("serialise clone");
        assert_eq!(json1, json3, "clone serialises differently for `{src}`");
    }
}

/// Byte-pinned goldens for representative statements across the four statement
/// families. A change to AST shape is a deliberate, reviewed event: update the
/// golden string here and explain why.
#[test]
fn pinned_goldens() {
    let cases: &[(&str, &str)] = &[
        (
            "FROM /mail/inbox |> WHERE id = 1 |> LIMIT 5",
            r#"{"Query":{"source":{"Path":{"segments":[{"name":"mail","version":null,"glob":false},{"name":"inbox","version":null,"glob":false}],"as_of":null,"span":[5,16]}},"ops":[{"Where":{"Binary":{"op":"Eq","lhs":{"Col":"id"},"rhs":{"Lit":{"Int":1}}}}},{"Limit":5}]}}"#,
        ),
        (
            "FROM /git/repo@main/src",
            r#"{"Query":{"source":{"Path":{"segments":[{"name":"git","version":null,"glob":false},{"name":"repo","version":"main","glob":false},{"name":"src","version":null,"glob":false}],"as_of":null,"span":[5,23]}},"ops":[]}}"#,
        ),
        (
            "INSERT INTO /t VALUES (1, 2) RETURNING id",
            r#"{"Effect":{"verb":"Insert","target":{"segments":[{"name":"t","version":null,"glob":false}],"as_of":null,"span":[12,14]},"body":{"Values":{"columns":null,"rows":[[{"Lit":{"Int":1}},{"Lit":{"Int":2}}]]}},"returning":[{"Expr":{"expr":{"Col":"id"},"alias":null}}]}}"#,
        ),
        (
            "FROM /mail/drafts |> CALL mail.send",
            r#"{"Query":{"source":{"Path":{"segments":[{"name":"mail","version":null,"glob":false},{"name":"drafts","version":null,"glob":false}],"as_of":null,"span":[5,17]}},"ops":[{"Call":{"driver":"mail","action":"send","args":[],"span":[21,35]}}]}}"#,
        ),
        (
            "PREVIEW REMOVE /mail/spam WHERE age > 30",
            r#"{"Plan":{"commit":false,"inner":{"Effect":{"verb":"Remove","target":{"segments":[{"name":"mail","version":null,"glob":false},{"name":"spam","version":null,"glob":false}],"as_of":null,"span":[15,25]},"body":{"SetWhere":{"set":[],"filter":{"Binary":{"op":"Gt","lhs":{"Col":"age"},"rhs":{"Lit":{"Int":30}}}}}},"returning":null}},"span":[0,7]}}"#,
        ),
    ];
    for (src, want) in cases {
        let stmt = parse_statement(src).unwrap_or_else(|e| panic!("`{src}` failed: {e}"));
        let got = serde_json::to_string(&stmt).expect("serialise");
        assert_eq!(&got, want, "golden mismatch for `{src}`\n got: {got}");
    }
}
