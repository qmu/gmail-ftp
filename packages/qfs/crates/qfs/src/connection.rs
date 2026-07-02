//! The `qfs connection` composition root: the real credential-store I/O that backs
//! `qfs connection add/list/use/remove`, injected into `qfs-cmd` as the [`qfs_cmd::ConnectionLauncher`].
//!
//! `qfs-cmd` may not depend on the concrete `qfs-secrets` backend (the dep_direction guard), so —
//! exactly like the shell / serve / describe launchers — the binary owns this and `qfs-cmd` only
//! parses the verb and calls in.
//!
//! ## Security (RFD §10)
//! - The credential **value** is read from **stdin**, never from argv (argv leaks into shell
//!   history and `ps`).
//! - Credentials live in the envelope-encrypted SQLite **Project DB** ([`crate::secret_store`]):
//!   a random data-key (DEK) encrypts each secret value (ChaCha20-Poly1305), and the DEK is wrapped
//!   under a key derived from the `QFS_PASSPHRASE` env var (argon2id) — the t43 replacement for the
//!   old file vault. The active-connection selection lives in the DB's `active_account` table (no
//!   passphrase needed — selectors only). Secrets are never printed, logged, or echoed.

use std::io::Read;

use qfs_cmd::ConnectionAction;
use qfs_identity::{IdentityStore, SoleUser};
use qfs_secrets::{is_cloud_driver, ConnectionId, CredentialKey, DriverId, Secret, Secrets};
use qfs_store::audit::AuditEvent;
use rusqlite::{Connection, OptionalExtension};

use crate::secret_store::{self, SqliteSecrets};

/// The injected connection launcher. Returns the process exit code (`0` ok, `1` on a structured,
/// secret-free error). Never panics.
#[must_use]
pub fn run_connection(action: &ConnectionAction) -> i32 {
    match run_inner(action) {
        Ok(msg) => {
            eprintln!("qfs: {msg}");
            0
        }
        Err(e) => {
            eprintln!("qfs: error: {e}");
            1
        }
    }
}

/// Open the migrated Project DB and return its **owned** connection (the t42 seam). The connection
/// carries the t43 secret-store schema; callers either move it into [`SqliteSecrets`] (the credential
/// path) or use it directly for the passphrase-free `active_account` table.
fn open_project_conn() -> Result<Connection, String> {
    let proj = crate::store::open_project_db()
        .map_err(|e| format!("opening the project database: {e}"))?
        .ok_or("cannot determine the project database path (set HOME or XDG_CONFIG_HOME)")?;
    Ok(proj.into_db().into_connection())
}

/// Has the credential store already been initialized on this host? True once the single
/// `secret_meta` row exists (written by the first [`SqliteSecrets::open_or_init`]). Passphrase-free —
/// it reads only a metadata presence flag, never key material — so it can decide *before* we know
/// the passphrase whether this is a first-run store-creation (prompt + confirm) or an unlock.
fn store_initialized(conn: &Connection) -> bool {
    conn.query_row("SELECT 1 FROM secret_meta WHERE id = 1", [], |_| Ok(()))
        .optional()
        .ok()
        .flatten()
        .is_some()
}

/// A passphrase entered at an interactive prompt EARLIER in this same process. The store is opened
/// many times per process — once per credentialed driver the shell binds at startup ([`commit`]'s
/// `networked_credential` opens it before it even knows if the driver is configured), plus every
/// read/commit leg — so without this an interactive session would prompt several times. Caching the
/// first-entered passphrase makes `qfs` (the interactive shell especially) prompt AT MOST ONCE and
/// reuse it for the whole session — the in-memory equivalent of `read -rs QFS_PASSPHRASE; export`,
/// except the secret never touches your shell env or history. Process-scoped only: it can never
/// reach a *sibling* `qfs` invocation (a child cannot mutate its parent shell), which is why a
/// multi-command workflow wants the shell, not repeated one-shots. Not populated from
/// `QFS_PASSPHRASE` (that path needs no cache) and never logged.
static PROMPTED_PASSPHRASE: std::sync::OnceLock<Secret> = std::sync::OnceLock::new();

/// Resolve the passphrase that unlocks (or initializes) the credential store. `QFS_PASSPHRASE` is
/// the fast path automation always takes; else a passphrase already entered this process is reused
/// ([`PROMPTED_PASSPHRASE`]); else a human at a terminal is PROMPTED (echo off), so no one has to
/// `export QFS_PASSPHRASE` by hand. On a brand-new store the prompt confirms twice (a typo would
/// otherwise lock the operator out of their own vault); on an existing store it asks once.
/// Non-interactive + unset stays the same clear error automation already relies on.
fn resolve_store_passphrase(conn: &Connection) -> Result<Secret, String> {
    match std::env::var("QFS_PASSPHRASE") {
        Ok(pass) if !pass.is_empty() => return Ok(Secret::from(pass)),
        Ok(_) => return Err("QFS_PASSPHRASE is empty".into()),
        Err(_) => {}
    }
    // Reuse a passphrase already entered this process (prompt once per session, not per driver).
    if let Some(cached) = PROMPTED_PASSPHRASE.get() {
        return Ok(Secret::from(cached.expose().to_vec()));
    }
    if !crate::tty::is_interactive() {
        return Err("QFS_PASSPHRASE is not set — export it (or run qfs in a terminal to be \
                    prompted) to unlock the encrypted credential store"
            .into());
    }
    let entered = if store_initialized(conn) {
        crate::tty::prompt_secret("QFS passphrase (unlocks your local credential store): ")?
    } else {
        eprintln!(
            "Welcome to qfs. Setting up your encrypted credential store on this machine — choose a \
             passphrase you'll reuse to unlock it (it never leaves this host)."
        );
        crate::tty::prompt_secret_confirmed("Choose a passphrase: ", "Confirm passphrase: ")?
    };
    // Cache for the rest of this process so later store-opens don't re-prompt. First writer wins;
    // a lost race just means the redundant entry is dropped (both are the same passphrase anyway).
    let _ = PROMPTED_PASSPHRASE.set(Secret::from(entered.expose().to_vec()));
    Ok(entered)
}

/// Open the envelope-encrypted SQLite credential store: open + migrate the Project DB, then unlock
/// (or initialize) the envelope with the resolved passphrase (`QFS_PASSPHRASE` or an interactive
/// prompt — see [`resolve_store_passphrase`]).
fn open_store() -> Result<SqliteSecrets, String> {
    let conn = open_project_conn()?;
    let pass = resolve_store_passphrase(&conn)?;
    SqliteSecrets::open_or_init(conn, &pass)
        .map_err(|e| format!("opening the credential store: {e}"))
}

/// Open the credential store for the **commit resolver** (read path): the same envelope-encrypted
/// SQLite store `connection add` writes to, when `QFS_PASSPHRASE` + the Project DB are both available.
/// Returns `None` (best-effort, never an error) when the store cannot be unlocked — the commit
/// registry then falls back to the env-var store, and a missing credential surfaces lazily as a
/// clear per-leg auth error rather than a panic. Never logs the passphrase.
#[must_use]
pub fn open_store_for_commit() -> Option<SqliteSecrets> {
    open_store().ok()
}

/// The persisted active connection name for `driver`, read from the Project DB's `active_account`
/// table (selectors only — no secret, so no passphrase is needed to read it). This is the same
/// selection `qfs connection use <driver> <connection>` writes; the commit resolver consumes it to
/// pick which credential to apply with. Returns `None` when unset/unreadable.
#[must_use]
pub fn active_connection(driver: &str) -> Option<String> {
    let conn = open_project_conn().ok()?;
    secret_store::db_get_active(&conn, driver)
}

/// Is `driver` one of the three Google drivers that authenticate through the shared
/// `qfs-google-auth` OAuth base (one consent serves all three)? Gates the opt-in interactive consent
/// branch in `connection add`. Note these are the CONSENT driver names (`gmail`/`gdrive`/`ga`),
/// distinct from the runtime registry ids (`mail`/`drive`/`ga`).
fn is_google_driver(driver: &str) -> bool {
    matches!(driver, "gmail" | "gdrive" | "ga")
}

fn cred_key(driver: &str, connection: &str) -> Result<CredentialKey, String> {
    let conn_id =
        ConnectionId::new(connection).map_err(|e| format!("invalid connection name: {e:?}"))?;
    Ok(CredentialKey::new(DriverId(driver.to_string()), conn_id))
}

/// t54 / M4 — the **sign-in mandatory** gate for a cloud driver. A cloud connection is unusable for
/// an unauthenticated operator (decision B/C: fail closed), so `connection add`/`use` for a cloud
/// driver first resolves the signed-in identity from the System-DB identity store (t45). Returns the
/// operator's identity (their primary email) to record on the consent grant, or a structured,
/// secret-free error naming the remedy.
///
/// Sessions (t46) are not yet wired into the CLI, so "signed in" today means **a signed-up identity
/// exists on this host**: exactly one user resolves unambiguously; many users without a session can't
/// be attributed, so we fail closed and ask for an explicit identity rather than guessing.
///
/// OPEN PRODUCT DECISION (flagged, not guessed — roadmap §3.1 talks teams, not the solo case): does a
/// solo single-user laptop still need to sign in for a cloud connection? Today we apply the rule
/// uniformly (fail closed) — the safe default — and leave relaxing it for the solo case to a product
/// call rather than baking an implicit exception here.
fn require_signed_in(driver: &str) -> Result<String, String> {
    let store = crate::identity::open_identity_store()?;
    match store
        .sole_user()
        .map_err(|e| format!("checking sign-in status: {e}"))?
    {
        SoleUser::One(u) => Ok(u.primary_email),
        SoleUser::None => Err(format!(
            "cloud driver '{driver}' requires sign-in — run `qfs identity signup <email>` first \
             (cloud connections are unusable for an unauthenticated operator)"
        )),
        SoleUser::Many => Err(format!(
            "cloud driver '{driver}' requires a signed-in identity, but this host has multiple users \
             and no session yet — sign in as a specific identity before adding a cloud connection"
        )),
    }
}

/// The OAuth scope a cloud connection's consent is recorded against — the driver's minimum scope set
/// (metadata, a §10 hint, never a token). Recorded on the consent grant so a later under-scoped use
/// is diagnosable; the live token negotiation that actually obtains these scopes is the driver's
/// OAuth client (`crates/google-auth`), out of band of this metadata.
fn consent_scope(driver: &str) -> &'static str {
    match driver {
        "gmail" => "gmail.readonly",
        "gdrive" => "drive.readonly",
        "ga" => "analytics.readonly",
        "github" => "repo",
        "slack" => "channels:read",
        "objstore" => "storage.read",
        "cf" => "account.read",
        _ => "",
    }
}

/// The t76 audit event a rotation / revocation / re-key emits — metadata ONLY (the verb, the
/// `<driver>/<connection>` selector, the `/sys/connections` surface, a timestamp), **never** a
/// secret. Kept as a pure builder so the emitted shape is unit-testable over an explicit System DB.
fn connection_audit_event(verb: &str, connection: &str) -> AuditEvent {
    AuditEvent {
        actor: "cli".to_string(),
        connection: connection.to_string(),
        verb: verb.to_string(),
        path: "/sys/connections".to_string(),
        committed: true,
        ts: now_rfc3339(),
    }
}

/// Append a connection rotation/revocation event onto the t76 hash chain (best-effort, exactly like
/// the commit path's `emit_audit`): a missing/unavailable System DB is logged at debug and never
/// breaks the operation. Secret-free — the event carries selectors + metadata only.
fn emit_connection_audit(verb: &str, connection: &str) {
    let event = connection_audit_event(verb, connection);
    match crate::store::open_system_db() {
        Ok(Some(sys)) => {
            if let Err(e) = crate::audit::append_event(&sys, event) {
                tracing::debug!(target: "qfs::audit", "connection audit append failed (continuing): {e}");
            }
        }
        Ok(None) => {}
        Err(e) => {
            tracing::debug!(target: "qfs::audit", "connection audit skipped (system DB unavailable): {e}");
        }
    }
}

/// The current UTC time as an RFC3339 string for an audit event's `ts` (mirrors `commit::now_rfc3339`
/// — a clock read can fail to format only on an impossible date; fall back to the epoch, never panic).
fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Read a single secret value from stdin (never argv — argv leaks into shell history and `ps`),
/// trimming the trailing newline. Used by `connection add`/`rotate`/`rekey` for the credential-input
/// path (§4.5 — a secret never enters a qfs statement). `what` names the value for the empty-input
/// error so the operator knows what to pipe.
fn read_secret_from_stdin(what: &str, example: &str) -> Result<String, String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| format!("reading the {what} from stdin: {e}"))?;
    let value = buf.trim_end_matches(['\n', '\r']).to_string();
    if value.is_empty() {
        return Err(format!("no {what} on stdin — pipe it, e.g. `{example}`"));
    }
    Ok(value)
}

fn run_inner(action: &ConnectionAction) -> Result<String, String> {
    match action {
        ConnectionAction::Add { driver, connection } => {
            // t54 / M4 — sign-in MANDATORY for a cloud driver. Resolve (and require) the signed-in
            // identity BEFORE touching the secret store or stdin, so an unauthenticated operator
            // fails closed up front (decision B/C). Local drivers are ungated (the M4 rule is a
            // cloud-tier concern only); `subject` is `None` for them.
            let subject = if is_cloud_driver(&DriverId(driver.clone())) {
                Some(require_signed_in(driver)?)
            } else {
                None
            };
            // Google drivers (gmail/gdrive/ga): OPT-IN interactive loopback browser consent
            // (QFS_GOOGLE_CONSENT). When set, run the real `qfs_google_auth::authorize` flow (the
            // documented seam in `crate::google`) INSTEAD of reading a refresh token from stdin: it
            // builds the OAuth client over the production transport, opens the consent URL, captures
            // the redirect code, exchanges it, and persists the refresh token under
            // `google:<email>:refresh_token` (one consent serves all three Google drivers). The
            // browser open + the human approval are interactive and NOT hermetically testable, so
            // this path is never exercised by a test; the default stdin path below stays the tested,
            // out-of-band provisioning route (so green never depends on this round-trip).
            if is_google_driver(driver) && std::env::var(crate::google::GOOGLE_CONSENT_ENV).is_ok()
            {
                let store = open_store()?;
                let store_arc: std::sync::Arc<dyn Secrets> = std::sync::Arc::new(store);
                let email = crate::google::run_google_consent(driver, store_arc)
                    .map_err(|e| format!("google consent failed: {e}"))?;
                // Record the M4 consent against the (driver, connection) selector — the load-bearing
                // state the commit-time bind gate consults (selectors + metadata only, never a token).
                if let Some(subject) = subject {
                    let proj = open_project_conn()?;
                    secret_store::db_record_consent(
                        &proj,
                        driver,
                        connection,
                        &subject,
                        consent_scope(driver),
                    )
                    .map_err(|e| format!("recording consent: {e}"))?;
                    return Ok(format!(
                        "authorized {driver}/{connection} for {email} (refresh token stored under \
                         google:{email}:refresh_token; consent granted by {subject})"
                    ));
                }
                return Ok(format!("authorized {driver}/{connection} for {email}"));
            }
            let store = open_store()?;
            let key = cred_key(driver, connection)?;
            // The credential value comes from stdin — never argv. For a cloud driver this is the
            // refresh token provisioned out of band (the wasm/refresh-only path the OAuth client
            // also feeds); the interactive loopback consent flow is the driver's native-only
            // `crates/google-auth` `authorize()` and is not exercised here (no network in this path).
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| format!("reading the secret from stdin: {e}"))?;
            let value = buf.trim_end_matches(['\n', '\r']).to_string();
            if value.is_empty() {
                return Err(
                    "no secret on stdin — pipe it, e.g. `printf %s \"$TOKEN\" | qfs connection add mail work`"
                        .into(),
                );
            }
            store
                .put(&key, Secret::from(value))
                .map_err(|e| format!("storing the credential: {e}"))?;
            // For a cloud driver, RECORD the consent against the connection (selectors + metadata
            // only — never the token, which the store sealed above). This is the load-bearing M4
            // state the commit-time bind gate consults to let the driver proceed.
            if let Some(subject) = subject {
                let proj = open_project_conn()?;
                secret_store::db_record_consent(
                    &proj,
                    driver,
                    connection,
                    &subject,
                    consent_scope(driver),
                )
                .map_err(|e| format!("recording consent: {e}"))?;
                return Ok(format!(
                    "stored credential and recorded consent for {driver}/{connection} (granted by {subject})"
                ));
            }
            Ok(format!("stored credential for {driver}/{connection}"))
        }
        ConnectionAction::List { driver } => {
            // Declared (non-secret) connections from the connections.qfs — these are plain config,
            // so they list WITHOUT a passphrase. Selectors + the secret REFERENCE only, never a value.
            let declared: Vec<_> = crate::connections_config::declared_connections()
                .into_iter()
                .filter(|c| driver.as_deref().is_none_or(|d| c.driver == d))
                .collect();
            for c in &declared {
                let secret = c
                    .secret_ref
                    .as_deref()
                    .map_or(String::new(), |r| format!("\tsecret {r}"));
                println!("{}/{}\t(declared){secret}", c.driver, c.name);
            }
            // Stored credentials — the encrypted vault behind `vault:` references. Needs the
            // passphrase; when it can't open we still show the declared connections (the vault is
            // just locked) unless there is nothing else to show, in which case surface the error.
            let stored = match open_store() {
                Ok(store) => {
                    let filter = driver.as_ref().map(|d| DriverId(d.clone()));
                    let recs = store
                        .list(filter.as_ref())
                        .map_err(|e| format!("listing connections: {e}"))?;
                    // Selectors + metadata only — never a credential.
                    for r in &recs {
                        println!("{}/{}\t{}", r.driver.0, r.connection, r.created_at);
                    }
                    recs.len()
                }
                Err(e) if declared.is_empty() => return Err(e),
                Err(_) => 0,
            };
            if declared.is_empty() && stored == 0 {
                return Ok("no connections configured".into());
            }
            Ok(format!(
                "{} declared + {} stored connection(s)",
                declared.len(),
                stored
            ))
        }
        ConnectionAction::ImportEnv => {
            // Print the CREATE CONNECTION declarations equivalent to the current QFS_SQL_*/QFS_GIT_*
            // env vars — the one-command migration off the deprecated convention. No secret is read
            // (SQLite/git connections carry none); the output is paste-ready for a connections.qfs.
            let decls = crate::connections_config::import_env_declarations();
            if decls.is_empty() {
                return Ok("no QFS_SQL_* / QFS_GIT_* env vars to import".into());
            }
            println!("{decls}");
            Ok("printed the equivalent CREATE CONNECTION declarations".into())
        }
        ConnectionAction::Remove { driver, connection } => {
            let store = open_store()?;
            let key = cred_key(driver, connection)?;
            store
                .remove(&key)
                .map_err(|e| format!("removing the credential: {e}"))?;
            Ok(format!("removed {driver}/{connection} (idempotent)"))
        }
        ConnectionAction::Rotate { driver, connection } => {
            // t79 — re-mint the secret. Cloud drivers carry the same sign-in gate as `add` (a cloud
            // connection is unusable for an unauthenticated operator); resolve identity BEFORE stdin.
            if is_cloud_driver(&DriverId(driver.clone())) {
                let _ = require_signed_in(driver)?;
            }
            let store = open_store()?;
            let key = cred_key(driver, connection)?;
            // The NEW credential value comes from stdin — never argv (§4.5).
            let value = read_secret_from_stdin(
                "new secret",
                &format!("printf %s \"$TOKEN\" | qfs connection rotate {driver} {connection}"),
            )?;
            store
                .rotate(&key, Secret::from(value))
                .map_err(|e| format!("rotating the credential: {e}"))?;
            emit_connection_audit("ROTATE", &format!("{driver}/{connection}"));
            Ok(format!(
                "rotated {driver}/{connection} (secret re-minted; any revocation cleared)"
            ))
        }
        ConnectionAction::Revoke { driver, connection } => {
            // t79 — mark the connection unresolvable. A later bind fails closed (the secret is never
            // returned); other connections keep working. Re-minting (`rotate`) restores use.
            let store = open_store()?;
            let key = cred_key(driver, connection)?;
            store
                .revoke(&key)
                .map_err(|e| format!("revoking the connection: {e}"))?;
            emit_connection_audit("REVOKE", &format!("{driver}/{connection}"));
            Ok(format!(
                "revoked {driver}/{connection} (it can no longer resolve until re-minted with `qfs connection rotate`)"
            ))
        }
        ConnectionAction::Rekey => {
            // t79 — DEK re-wrap on a passphrase change. The OLD passphrase is `QFS_PASSPHRASE` (the
            // one that opened the store); the NEW passphrase comes from stdin (never argv). One
            // re-wrap of the wrapped-DEK — existing secrets stay decryptable, the old passphrase
            // stops unlocking. A wrong old passphrase cannot reach here (the store would not open).
            let store = open_store()?;
            let old = std::env::var("QFS_PASSPHRASE")
                .map_err(|_| "QFS_PASSPHRASE is not set".to_string())?;
            let new = read_secret_from_stdin(
                "new passphrase",
                "printf %s \"$NEWPASS\" | qfs connection rekey",
            )?;
            store
                .rewrap_passphrase(&Secret::from(old), &Secret::from(new))
                .map_err(|e| format!("re-wrapping the data key: {e}"))?;
            emit_connection_audit("REKEY", "store");
            Ok("re-wrapped the credential store under the new passphrase — set QFS_PASSPHRASE to the new value for the next run".into())
        }
        ConnectionAction::Use { driver, connection } => {
            // Validate the names, then persist the active selection into the Project DB's
            // `active_account` table (selectors only — no passphrase needed). The commit resolver
            // reads it back via `active_connection()`.
            let _ = cred_key(driver, connection)?;
            // t54 / M4 — selecting a cloud connection is gated on sign-in too: an unauthenticated
            // operator may not make a cloud connection active (fail closed).
            if is_cloud_driver(&DriverId(driver.clone())) {
                let _ = require_signed_in(driver)?;
            }
            let conn = open_project_conn()?;
            secret_store::db_set_active(&conn, driver, connection)
                .map_err(|e| format!("setting the active connection: {e}"))?;
            Ok(format!(
                "active connection for {driver} set to {connection}"
            ))
        }
        // t100020 (the CONNECT model): bind a defined PATH to a driver + credential reference (or an
        // alias). Direct Project-DB I/O — the twin of the `CONNECT` statement. No passphrase: the
        // binding is metadata + a secret REFERENCE, never a value.
        ConnectionAction::Connect {
            path,
            driver,
            at,
            secret_ref,
            alias_of,
        } => run_connect(
            path,
            driver.as_deref(),
            at.as_deref(),
            secret_ref.as_deref(),
            alias_of.as_deref(),
        ),
        ConnectionAction::Disconnect { path } => {
            let conn = open_project_conn()?;
            let n = crate::path_binding::db_remove_binding(&conn, path)
                .map_err(|e| format!("removing the defined path: {e}"))?;
            Ok(if n == 0 {
                format!("{path} was not connected (idempotent)")
            } else {
                format!("disconnected {path}")
            })
        }
        ConnectionAction::ListPaths => {
            let conn = open_project_conn()?;
            let rows = crate::path_binding::db_list_bindings(&conn)
                .map_err(|e| format!("listing defined paths: {e}"))?;
            for r in &rows {
                // Selectors + metadata only — the `secret_ref` is a REFERENCE (env:/vault:), never a
                // value. An alias renders its target; a full connect its driver (+ optional locator).
                if let Some(target) = &r.alias_of {
                    println!("{}\t-> {target}\t(alias)", r.path);
                } else {
                    let driver = r.driver_id.as_deref().unwrap_or("?");
                    let at = r
                        .at_locator
                        .as_deref()
                        .map_or(String::new(), |a| format!("\tat {a}"));
                    let secret = r
                        .secret_ref
                        .as_deref()
                        .map_or(String::new(), |s| format!("\tsecret {s}"));
                    println!("{}\t{driver}{at}{secret}", r.path);
                }
            }
            Ok(if rows.is_empty() {
                "no defined paths (use `qfs connect`)".into()
            } else {
                format!("{} defined path(s)", rows.len())
            })
        }
    }
}

/// Bind a defined path (`qfs connect`): validate the arms (exactly one of `driver` / `alias_of`),
/// then UPSERT the binding into the Project DB `path_binding` table. A `vault:`/`env:` secret is
/// stored as a REFERENCE only (resolved at use time) — nothing secret touches argv or the row.
fn run_connect(
    path: &str,
    driver: Option<&str>,
    at: Option<&str>,
    secret_ref: Option<&str>,
    alias_of: Option<&str>,
) -> Result<String, String> {
    if !path.starts_with('/') {
        return Err(format!("a defined path must be absolute, got `{path}`"));
    }
    let conn = open_project_conn()?;
    match (driver, alias_of) {
        (Some(_), Some(_)) => {
            Err("give either --driver (full connect) or --alias-of (alias), not both".into())
        }
        (None, None) => {
            Err("a full connect needs --driver (or --alias-of <path> for an alias)".into())
        }
        (Some(driver), None) => {
            crate::path_binding::db_upsert_binding(&conn, path, driver, at, secret_ref)
                .map_err(|e| format!("connecting {path}: {e}"))?;
            Ok(format!("connected {path} -> {driver}"))
        }
        (None, Some(target)) => {
            crate::path_binding::db_upsert_alias(&conn, path, target).map_err(|e| {
                // A foreign-key failure means the alias target is not a defined path (fail-closed).
                if matches!(&e, rusqlite::Error::SqliteFailure(err, _)
                    if err.code == rusqlite::ErrorCode::ConstraintViolation)
                {
                    format!("the alias target `{target}` is not a defined path — connect it first")
                } else {
                    format!("aliasing {path}: {e}")
                }
            })?;
            Ok(format!("connected {path} -> {target} (alias)"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cred_key_rejects_an_invalid_connection_name() {
        assert!(cred_key("mail", "").is_err());
        let k = cred_key("mail", "work").expect("valid");
        assert_eq!(k.driver.0, "mail");
        assert_eq!(k.connection.as_str(), "work");
    }

    /// `store_initialized` is the passphrase-free flag that decides first-run (create + confirm) vs
    /// unlock in the interactive prompt: false on a migrated-but-empty Project DB, true once the
    /// single `secret_meta` row exists (what the first `open_or_init` writes).
    #[test]
    fn store_initialized_reflects_the_secret_meta_row() {
        use qfs_store::{MemorySource, ProjectDb};
        let conn = ProjectDb::open(&MemorySource)
            .unwrap()
            .into_db()
            .into_connection();
        assert!(!store_initialized(&conn), "a fresh store is not initialized");

        // The first real open writes secret_meta id=1; emulate exactly that row (bytes are opaque to
        // the presence check) and confirm the flag flips.
        conn.execute(
            "INSERT INTO secret_meta (id, wrapped_dek, kdf_salt) VALUES (1, ?1, ?2)",
            rusqlite::params![vec![0u8; 4], vec![0u8; 4]],
        )
        .unwrap();
        assert!(store_initialized(&conn), "an established store is initialized");
    }

    /// The active-connection selection now round-trips through the Project DB's `active_account`
    /// table (replacing the old `.active` sidecar): `use` UPSERTs, the resolver reads back, and
    /// per-driver rows stay independent (last-writer-wins). Exercised over the same DB seam the
    /// binary uses (`db_set_active` / `db_get_active`).
    #[test]
    fn active_selection_round_trips_through_the_db_table() {
        use qfs_store::{MemorySource, ProjectDb};
        let conn = ProjectDb::open(&MemorySource)
            .unwrap()
            .into_db()
            .into_connection();

        assert!(secret_store::db_get_active(&conn, "mail").is_none());
        secret_store::db_set_active(&conn, "mail", "work").unwrap();
        secret_store::db_set_active(&conn, "s3", "prod").unwrap();
        // Replacing mail's connection must NOT affect s3 and must not duplicate the row.
        secret_store::db_set_active(&conn, "mail", "personal").unwrap();

        assert_eq!(
            secret_store::db_get_active(&conn, "mail").as_deref(),
            Some("personal")
        );
        assert_eq!(
            secret_store::db_get_active(&conn, "s3").as_deref(),
            Some("prod")
        );
    }

    /// t79: a rotation and a revocation each append a t76 audit row carrying selectors + metadata
    /// ONLY (the verb, the `<driver>/<connection>` selector, the `/sys/connections` surface) — never
    /// a secret. Exercised over an explicit System DB (hermetic) using the exact event the handlers
    /// emit (`connection_audit_event` + `append_event`).
    #[test]
    fn rotation_and_revocation_append_an_audit_row() {
        use qfs_store::{FileSource, SystemDb};
        let dir = tempfile::tempdir().unwrap();
        let sys = SystemDb::open(&FileSource::new(dir.path().join("system.db"))).unwrap();

        crate::audit::append_event(&sys, connection_audit_event("ROTATE", "github/team")).unwrap();
        crate::audit::append_event(&sys, connection_audit_event("REVOKE", "github/leaver"))
            .unwrap();

        let tail = crate::audit::recent_tail(&sys).unwrap();
        assert_eq!(tail.len(), 2, "both events landed on the chain");
        assert_eq!(tail[0].event.verb, "ROTATE");
        assert_eq!(tail[0].event.connection, "github/team");
        assert_eq!(tail[0].event.path, "/sys/connections");
        assert_eq!(tail[1].event.verb, "REVOKE");
        assert_eq!(tail[1].event.connection, "github/leaver");
        // The chain links the two events (the second's prev is the first's hash).
        assert_eq!(tail[1].prev_hash, tail[0].hash);
        // The audit rows carry no secret material — selectors + metadata only.
        let dump = format!("{tail:?}");
        assert!(!dump.contains("ghp_") && !dump.contains("token"));
    }
}
