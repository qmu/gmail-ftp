//! Planner-owned **E2E / external-interface** black-box validation of the t29 one-shot CLI.
//!
//! This is NOT a unit test and NOT a code review: every scenario drives the system from the
//! OUTSIDE — almost all by spawning the REAL `cfs` binary as a subprocess and asserting
//! stdout / stderr / exit code, exactly the contract an AI agent or a shell script depends on.
//! No live creds, no network.
//!
//! ## Why this test lives in `cfs-cmd`, not in the `cfs` binary crate
//! The `cfs` binary crate is a thin entrypoint guarded (by `tests/dep_direction.rs`) to depend on
//! `cfs-cmd` ONLY among workspace crates — including dev-deps, which `cargo metadata` counts. A
//! subprocess-driving test there would need `serde_json`/`cfs-exec` dev-deps and break that
//! invariant. So the E2E lives one layer down in `cfs-cmd` (which already owns the run dispatch
//! and its argv contract), and locates the built `cfs` binary next to the test runner.
//!
//! ## Why scenario 1 uses the cfs-exec black-box API, not the binary
//! The binary's `ReadRegistry` is intentionally EMPTY at this E0 stage (driver registration is the
//! E4 seam), so `FROM /<src>` through the binary resolves to a structured capability error rather
//! than rows. To exercise the real parse→resolve→plan→scan→residual→rows path with actual data,
//! that one scenario drives the executor's public black-box API (`run_oneshot`/`block_on_read`)
//! against a Planner-owned in-memory fake driver — and this header SAYS SO.
//!
//! Scenario map (ticket acceptance criteria):
//!  1. Headline read path + residual truthfulness (t20 closure) — via cfs-exec black-box API.
//!  2. Output-format defaults: table on a (pseudo-)TTY, json when piped; explicit flag wins.
//!  3. PREVIEW/COMMIT gate: pure/non-destructive preview exit 0; destructive-set exit 4;
//!     --commit / trailing COMMIT applies.
//!  4. Error contract + exit codes (parse=2, usage=2, capability=3) with the t01-superset
//!     envelope (`code` AND `kind`), and kind↔exit-code 1:1.
//!  5. Addressing: absolute accepted; relative rejected (usage, exit 2).
//!  6. stdout/stderr separation; --quiet suppresses progress but not the error body.
//!  7. --help snapshot stability for `cfs` and `cfs run`.
//!  8. Secret safety: no credential material reachable in the error DTO.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde_json::Value as Json;

// ===================================================================================
// Subprocess harness: locate and spawn the REAL `cfs` binary.
// ===================================================================================

/// Locate the built `cfs` binary. The integration-test runner lives in `target/<profile>/deps/`;
/// the binary is the sibling `target/<profile>/cfs`. Built on demand if missing.
fn cfs_bin() -> PathBuf {
    // current_exe() = target/<profile>/deps/e2e_cli-<hash>
    let mut dir = std::env::current_exe().expect("current_exe");
    dir.pop(); // deps/
    dir.pop(); // <profile>/
    let bin = dir.join(if cfg!(windows) { "cfs.exe" } else { "cfs" });
    if !bin.is_file() {
        // Build it via the same cargo the harness was invoked with.
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let status = Command::new(cargo)
            .args(["build", "-p", "cfs"])
            .status()
            .expect("build cfs");
        assert!(status.success(), "failed to build the cfs binary");
    }
    assert!(bin.is_file(), "cfs binary not found at {}", bin.display());
    bin
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Run `cfs <args...>` with an empty stdin and a quiet `RUST_LOG` (so tracing never pollutes the
/// machine streams under test). Returns the captured outcome.
fn cfs(args: &[&str]) -> Out {
    let mut child = Command::new(cfs_bin())
        .args(args)
        .env("RUST_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cfs binary");
    // Close stdin immediately (no positional `-`, so the child never reads it).
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("wait cfs");
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Run `cfs run -` feeding `stdin` to the child.
fn cfs_stdin(args: &[&str], stdin: &[u8]) -> Out {
    let mut child = Command::new(cfs_bin())
        .args(args)
        .env("RUST_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cfs binary");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(stdin)
        .expect("write child stdin");
    let out = child.wait_with_output().expect("wait cfs");
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn json(s: &str) -> Json {
    serde_json::from_str(s.trim()).unwrap_or_else(|e| panic!("not JSON ({e}): {s:?}"))
}

// ===================================================================================
// Scenario 1: headline read path + residual truthfulness (the t20 carry-over closure).
// Driven via the cfs-exec black-box API against a Planner-owned in-memory fake driver,
// because the binary's read registry is empty by design (see module docs).
// ===================================================================================

mod read_path {
    use super::*;
    use cfs_core::{AppliedEffect, ApplyError, EffectNode, PlanApplier};
    use cfs_core::{
        Archetype, Capabilities, CfsError, Column, ColumnType, DriverId, Engine, NodeDesc, Path,
        PushdownProfile, Row, RowBatch, Schema, Value,
    };
    use cfs_exec::{
        block_on_read, parse, run_oneshot, ExecCtx, OutputFormat, ReadDriver, ReadRegistry,
        StmtSource, Streams,
    };
    use cfs_pushdown::ScanNode;

    /// A fake mail source that DELIBERATELY OVER-RETURNS every row (PushdownProfile::None), so the
    /// engine's residual WHERE/LIMIT re-filter is the only thing that can restore correctness.
    /// This is the highest-value check: if the residual is wrong, wrong rows leak through.
    struct FakeMail {
        mount: String,
        rows: Vec<Row>,
    }

    fn schema() -> Schema {
        Schema::new(vec![
            Column::new("id", ColumnType::Int, false),
            Column::new("subject", ColumnType::Text, true),
        ])
    }

    impl FakeMail {
        fn new() -> Self {
            Self {
                mount: "/mail".to_string(),
                rows: vec![
                    Row::new(vec![Value::Int(1), Value::Text("hello".into())]),
                    Row::new(vec![Value::Int(2), Value::Text("spam".into())]),
                    Row::new(vec![Value::Int(3), Value::Text("world".into())]),
                    Row::new(vec![Value::Int(4), Value::Text("late".into())]),
                ],
            }
        }
    }

    #[derive(Default)]
    struct NoopApplier;
    impl PlanApplier for NoopApplier {
        fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
            Ok(AppliedEffect::new(node.id, 0))
        }
    }

    impl cfs_core::Driver for FakeMail {
        fn mount(&self) -> &str {
            &self.mount
        }
        fn describe(&self, _p: &Path) -> Result<NodeDesc, CfsError> {
            Ok(NodeDesc::new(Archetype::RelationalTable, schema()))
        }
        fn capabilities(&self, _p: &Path) -> Capabilities {
            Capabilities::none().select()
        }
        fn procedures(&self) -> &[cfs_core::ProcSig] {
            &[]
        }
        fn pushdown(&self) -> &PushdownProfile {
            &PushdownProfile::None
        }
        fn applier(&self) -> &dyn PlanApplier {
            Box::leak(Box::new(NoopApplier))
        }
    }

    #[async_trait::async_trait]
    impl ReadDriver for FakeMail {
        async fn scan(&self, _scan: &ScanNode) -> Result<RowBatch, CfsError> {
            // Honestly over-return: hand back ALL rows regardless of pushed WHERE/LIMIT.
            Ok(RowBatch::new(schema(), self.rows.clone()))
        }
    }

    fn engine() -> Engine {
        let mut e = Engine::new();
        e.mounts.register(Arc::new(FakeMail::new())).unwrap();
        e
    }
    fn reads() -> ReadRegistry {
        ReadRegistry::new().with(DriverId::new("mail"), Arc::new(FakeMail::new()))
    }

    fn ids(rows: &cfs_exec::RowSet) -> Vec<i64> {
        rows.rows
            .iter()
            .map(|r| match r.values[0] {
                Value::Int(i) => i,
                _ => -1,
            })
            .collect()
    }

    #[test]
    fn limit_residual_trims_over_returned_rows() {
        // FROM /mail/inbox |> LIMIT 1 — fake returns 4, residual LIMIT must trim to exactly 1.
        let (eng, rd) = (engine(), reads());
        let stmt = parse("FROM /mail/inbox |> LIMIT 1").unwrap();
        let rows = block_on_read(&stmt, &eng.mounts, &rd).unwrap();
        assert_eq!(rows.len(), 1, "LIMIT 1 must trim the over-returned scan");
        assert_eq!(ids(&rows), vec![1]);
        assert_eq!(rows.columns(), vec!["id", "subject"]);
    }

    #[test]
    fn where_residual_keeps_exactly_the_correct_rows_no_wrong_rows() {
        // The trap: a None-pushdown source hands back all 4 rows; WHERE id > 1 |> LIMIT 2 must
        // re-filter to EXACTLY ids [2,3] — not the over-returned [1,2,3,4], not [3,4].
        let (eng, rd) = (engine(), reads());
        let stmt = parse("FROM /mail/inbox |> WHERE id > 1 |> LIMIT 2").unwrap();
        let rows = block_on_read(&stmt, &eng.mounts, &rd).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            ids(&rows),
            vec![2, 3],
            "residual must keep exactly the right set"
        );
    }

    #[test]
    fn oneshot_read_json_envelope_is_rows_object_exit_zero() {
        // End-to-end through run_oneshot: stable {"rows":[{id,subject},…]} on stdout, exit 0,
        // nothing on stderr.
        let (eng, rd) = (engine(), reads());
        let ctx = ExecCtx {
            engine: &eng,
            reads: &rd,
        };
        let src = StmtSource::Expr("FROM /mail/inbox |> WHERE id > 2".to_string());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = {
            let mut s = Streams {
                out: &mut out,
                err: &mut err,
            };
            run_oneshot(&src, &ctx, OutputFormat::Json, false, &mut s).code()
        };
        assert_eq!(code, 0);
        assert!(err.is_empty(), "rows go to stdout, never stderr");
        let v = json(&String::from_utf8(out).unwrap());
        let arr = v["rows"].as_array().expect("rows array");
        assert_eq!(arr.len(), 2, "id > 2 keeps ids 3 and 4");
        assert_eq!(arr[0]["id"], 3);
        assert_eq!(arr[0]["subject"], "world");
    }
}

// ===================================================================================
// Scenario 4 + 5: error contract, exit codes, addressing — via the REAL binary.
// ===================================================================================

#[test]
fn parse_error_writes_kind_parse_to_stderr_exit_two() {
    let o = cfs(&["run", "-e", "this is not pipe sql", "--json"]);
    assert_eq!(o.code, 2, "parse error is exit 2");
    assert!(o.stdout.is_empty(), "no data on stdout for an error");
    let v = json(&o.stderr);
    assert_eq!(v["error"]["kind"], "parse");
    // t01-superset envelope: BOTH `code` and `kind` must be present.
    assert_eq!(v["error"]["code"], "parse_error");
    assert!(v["error"]["message"].is_string());
}

#[test]
fn unknown_source_is_capability_exit_three() {
    // No read driver registered → an absolute FROM resolves to a structured capability error.
    let o = cfs(&["run", "-e", "FROM /mail/inbox |> LIMIT 1", "--json"]);
    assert_eq!(o.code, 3, "unsupported-op/capability is exit 3");
    let v = json(&o.stderr);
    assert_eq!(v["error"]["kind"], "capability");
    assert_eq!(v["error"]["code"], "unknown_source");
}

#[test]
fn relative_path_is_usage_exit_two_with_offending_path() {
    let o = cfs(&["run", "-e", "FROM mail/inbox |> LIMIT 1", "--json"]);
    assert_eq!(o.code, 2, "relative path is a usage error, exit 2");
    let v = json(&o.stderr);
    assert_eq!(v["error"]["kind"], "usage");
    assert_eq!(v["error"]["code"], "usage");
    assert_eq!(
        v["error"]["path"], "mail/inbox",
        "the offending path is surfaced"
    );
}

#[test]
fn absolute_path_accepted_passes_addressing_gate() {
    // An absolute path passes the addressing gate (it fails LATER at capability, not at usage —
    // the proof addressing accepted it). Contrast with the relative-path usage error above.
    let o = cfs(&["run", "-e", "FROM /mail/inbox |> LIMIT 1", "--json"]);
    let v = json(&o.stderr);
    assert_ne!(
        v["error"]["kind"], "usage",
        "absolute path must pass addressing"
    );
    assert_eq!(v["error"]["kind"], "capability");
}

#[test]
fn stdin_source_is_read_and_addressing_validated() {
    // `cfs run -` reads the statement from stdin (the agent-pipeline path). A relative path fed
    // on stdin still hits the addressing gate (usage, exit 2) — proof the stdin source resolves.
    let o = cfs_stdin(&["run", "-", "--json"], b"FROM mail/inbox |> LIMIT 1");
    assert_eq!(o.code, 2);
    let v = json(&o.stderr);
    assert_eq!(v["error"]["kind"], "usage");
    assert_eq!(v["error"]["path"], "mail/inbox");
}

#[test]
fn kind_to_exit_code_is_one_to_one() {
    // Pin the kind↔exit-code map an agent branches on: one kind ⇒ one exit code.
    let cases: &[(&str, &str, i32)] = &[
        ("this is not pipe sql", "parse", 2),
        ("FROM mail/inbox |> LIMIT 1", "usage", 2),
        ("FROM /mail/inbox |> LIMIT 1", "capability", 3),
        ("REMOVE /mail/inbox", "commit_required", 4),
    ];
    for (stmt, kind, code) in cases {
        let o = cfs(&["run", "-e", stmt, "--json"]);
        assert_eq!(o.code, *code, "stmt {stmt:?} expected exit {code}");
        // commit_required renders its preview on stdout and the error on stderr.
        let v = json(&o.stderr);
        assert_eq!(
            v["error"]["kind"], *kind,
            "stmt {stmt:?} expected kind {kind}"
        );
    }
}

// ===================================================================================
// Scenario 3: PREVIEW / COMMIT gate — via the REAL binary (effect plans build with an
// empty engine; commit applies against the in-memory engine).
// ===================================================================================

#[test]
fn non_destructive_effect_previews_at_exit_zero_with_counts() {
    let o = cfs(&[
        "run",
        "-e",
        "INSERT INTO /mail/inbox VALUES (9, 'x')",
        "--json",
    ]);
    assert_eq!(o.code, 0, "a non-destructive preview is exit 0");
    assert!(o.stderr.is_empty(), "no error on a clean preview");
    let v = json(&o.stdout);
    assert_eq!(v["committed"], false, "PREVIEW is not committed");
    let rows = v["preview"]["rows"].as_array().expect("preview rows");
    assert_eq!(rows[0]["verb"], "INSERT");
    assert_eq!(
        rows[0]["affected"]["exact"], 1,
        "per-target affected count shown"
    );
}

#[test]
fn destructive_set_without_commit_exits_four_but_still_previews() {
    let o = cfs(&["run", "-e", "REMOVE /mail/inbox", "--json"]);
    assert_eq!(
        o.code, 4,
        "destructive set-wide plan without --commit is exit 4"
    );
    // The PREVIEW is rendered on STDOUT so the operator/agent sees the affected counts.
    let preview = json(&o.stdout);
    assert_eq!(preview["committed"], false);
    assert!(
        !preview["preview"]["irreversible"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the irreversible effect ids are surfaced in the preview"
    );
    // The commit_required error is on STDERR (stdout/stderr separation).
    let err = json(&o.stderr);
    assert_eq!(err["error"]["kind"], "commit_required");
}

#[test]
fn commit_flag_applies_against_in_memory_engine() {
    let o = cfs(&["run", "-e", "REMOVE /mail/inbox", "--json", "--commit"]);
    assert_eq!(o.code, 0, "--commit applies the plan");
    let v = json(&o.stdout);
    assert_eq!(v["committed"], true);
}

#[test]
fn trailing_commit_keyword_applies_without_the_flag() {
    // The CLI adds zero keywords: a trailing COMMIT (the engine's keyword) applies on its own.
    let o = cfs(&["run", "-e", "COMMIT REMOVE /mail/inbox", "--json"]);
    assert_eq!(o.code, 0);
    let v = json(&o.stdout);
    assert_eq!(v["committed"], true);
}

// ===================================================================================
// Scenario 2: output-format defaults — table on a (pseudo-)TTY, json when piped; flag wins.
// We pipe the subprocess (non-TTY) directly; the TTY default is exercised via `script`
// (a pty wrapper) when available, otherwise that single assertion is skipped (documented).
// ===================================================================================

#[test]
fn piped_default_is_json_no_flag() {
    // stdout is a pipe (captured) → json by default, no plan prompt, just the document.
    let o = cfs(&["run", "-e", "INSERT INTO /mail/inbox VALUES (9, 'x')"]);
    assert_eq!(o.code, 0);
    let v = json(&o.stdout);
    assert!(
        v["preview"].is_object(),
        "piped default renders machine JSON"
    );
}

#[test]
fn explicit_format_table_overrides_pipe_default() {
    let o = cfs(&[
        "run",
        "-e",
        "INSERT INTO /mail/inbox VALUES (9, 'x')",
        "--format",
        "table",
    ]);
    assert_eq!(o.code, 0);
    assert!(
        o.stdout.contains("PREVIEW") && o.stdout.contains("INSERT"),
        "explicit --format table wins even when piped: {:?}",
        o.stdout
    );
    assert!(
        serde_json::from_str::<Json>(o.stdout.trim()).is_err(),
        "table output is not JSON"
    );
}

#[test]
fn explicit_json_flag_is_machine_json() {
    let o = cfs(&[
        "run",
        "-e",
        "INSERT INTO /mail/inbox VALUES (9, 'x')",
        "--json",
    ]);
    assert_eq!(o.code, 0);
    assert!(json(&o.stdout)["preview"].is_object());
}

#[test]
fn tty_default_is_table_via_pty() {
    // Drive the binary under a pseudo-TTY so `IsTerminal` is true → default must be `table`.
    // Uses util-linux `script`; if absent, the assertion is skipped (and we say so).
    let Ok(script) = which("script") else {
        eprintln!("SKIP tty_default_is_table_via_pty: `script` (util-linux) not on PATH");
        return;
    };
    // `script -qec "<cmd>" /dev/null` runs <cmd> attached to a pty, capturing to /dev/null but
    // letting <cmd>'s own stdout flow to script's stdout (the pipe we capture).
    let cmd = format!(
        "{} run -e \"INSERT INTO /mail/inbox VALUES (9, 'x')\"",
        cfs_bin().display()
    );
    let out = Command::new(script)
        .args(["-qec", &cmd, "/dev/null"])
        .env("RUST_LOG", "off")
        .output()
        .expect("spawn script");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("PREVIEW") && stdout.contains("INSERT"),
        "on a TTY the default must be the human table/preview, got: {stdout:?}"
    );
    assert!(
        serde_json::from_str::<Json>(stdout.trim()).is_err(),
        "TTY default must not be JSON"
    );
}

/// Resolve an executable on PATH (tiny `which`, no extra dep).
fn which(bin: &str) -> Result<std::path::PathBuf, ()> {
    let path = std::env::var_os("PATH").ok_or(())?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(bin);
        if cand.is_file() {
            return Ok(cand);
        }
    }
    Err(())
}

// ===================================================================================
// Scenario 6: stdout/stderr separation + --quiet.
// ===================================================================================

#[test]
fn data_on_stdout_errors_on_stderr() {
    // A clean read-preview: data on stdout, nothing on stderr.
    let ok = cfs(&[
        "run",
        "-e",
        "INSERT INTO /mail/inbox VALUES (1, 'x')",
        "--json",
    ]);
    assert!(!ok.stdout.is_empty() && ok.stderr.is_empty());
    // An error: nothing on stdout, the error body on stderr.
    let bad = cfs(&["run", "-e", "FROM /mail/inbox |> LIMIT 1", "--json"]);
    assert!(
        bad.stdout.is_empty(),
        "no data on stdout for a capability error"
    );
    assert!(!bad.stderr.is_empty(), "the error body is on stderr");
}

#[test]
fn quiet_suppresses_progress_but_not_the_error_body() {
    // --quiet must NOT swallow the structured error (RFD §10): the agent still gets a body.
    let o = cfs(&[
        "run",
        "-e",
        "FROM /mail/inbox |> LIMIT 1",
        "--json",
        "--quiet",
    ]);
    assert_eq!(o.code, 3);
    let v = json(&o.stderr);
    assert_eq!(
        v["error"]["kind"], "capability",
        "--quiet keeps the error body"
    );
}

// ===================================================================================
// Scenario 7: --help snapshot stability for `cfs` and `cfs run`.
// ===================================================================================

#[test]
fn top_level_help_is_stable_and_exit_zero() {
    let o = cfs(&["--help"]);
    assert_eq!(o.code, 0, "--help exits 0");
    for needle in ["Usage: cfs", "run", "serve", "account", "--json"] {
        assert!(
            o.stdout.contains(needle),
            "`cfs --help` lost `{needle}`:\n{}",
            o.stdout
        );
    }
}

#[test]
fn run_help_is_stable_and_pins_the_agent_surface() {
    let o = cfs(&["run", "--help"]);
    assert_eq!(o.code, 0);
    // Pin the load-bearing flags/args an agent scripts against (a rename/removal fails CI).
    for needle in [
        "[STMT]", "--expr", "--format", "--commit", "--quiet", "stdin", "PREVIEW",
    ] {
        assert!(
            o.stdout.contains(needle),
            "`cfs run --help` lost `{needle}`:\n{}",
            o.stdout
        );
    }
}

// ===================================================================================
// Scenario 8: secret safety — no credential material can appear in the error DTO.
// ===================================================================================

#[test]
fn error_dto_is_whitelisted_fields_only_no_secret_leak() {
    // The structural guarantee (RFD §10): the error envelope is a FIXED whitelist — only
    // code, kind, message, and optional path/detail may ever appear. No credential-bearing
    // field can be added by construction, so no secret can ride out in the DTO. We also plant a
    // canary credential value in the environment and confirm it is never reflected into the
    // error body. (We do NOT scan for the noun "token": the parser legitimately says
    // "unexpected token", which is grammar terminology, not a credential.)
    const CANARY: &str = "CANARY-SECRET-9f3a7c-do-not-leak";
    let cases = [
        "this is not pipe sql",        // parse
        "FROM mail/inbox |> LIMIT 1",  // usage (carries a path field)
        "FROM /mail/inbox |> LIMIT 1", // capability
    ];
    for stmt in cases {
        // Plant the canary in the env the child inherits; it must never surface in the DTO.
        let child = Command::new(cfs_bin())
            .args(["run", "-e", stmt, "--json"])
            .env("RUST_LOG", "off")
            .env("CFS_FAKE_TOKEN", CANARY)
            .env("AWS_SECRET_ACCESS_KEY", CANARY)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn cfs");
        let out = child.wait_with_output().expect("wait cfs");
        let stderr = String::from_utf8_lossy(&out.stderr);

        let v = json(&stderr);
        let err = v["error"].as_object().expect("error object");
        for key in err.keys() {
            assert!(
                matches!(
                    key.as_str(),
                    "code" | "kind" | "message" | "path" | "detail"
                ),
                "error DTO leaked a non-whitelisted field `{key}` for {stmt:?}"
            );
        }
        assert!(
            !stderr.contains(CANARY),
            "the planted credential value leaked into the error DTO for {stmt:?}: {stderr}"
        );
    }
}

#[test]
fn account_stub_prints_no_credential_material() {
    // `account list` is an E0 stub, but it must never echo a credential. Plant a canary in the
    // env the child inherits and confirm neither stream reflects it (the account name is safe
    // metadata; the credential value is not).
    const CANARY: &str = "CANARY-SECRET-acct-do-not-leak";
    let child = Command::new(cfs_bin())
        .args(["--json", "account", "list"])
        .env("RUST_LOG", "off")
        .env("CFS_FAKE_TOKEN", CANARY)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cfs");
    let out = child.wait_with_output().expect("wait cfs");
    let blob =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(
        !blob.contains(CANARY),
        "account stub leaked the planted credential value: {blob}"
    );
}
