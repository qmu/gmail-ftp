//! ADR 0008 §1 — the **client-side hosts registry** and `qfs host` verb (EPIC 20260702120000 /
//! ticket 20260702120060). The CLI is a multi-host client; `local` is the implicit embedded host.
//!
//! This ticket reserves the surface WITHOUT the remote protocol (deferred per ADR §6 — "the first
//! remote-host ticket must own what `host login` speaks"). `qfs host login <url>` **records** a
//! host with **zero network I/O**; binding a mount to a remote host is refused at CONNECT time
//! ("remote hosts are not yet executable" — [`require_known_host`]) rather than left a
//! non-functional mount. The store is the System DB `hosts` table (migration #13); `local` is
//! seeded there and cannot be removed.
//!
//! NOTE the name collision with [`crate::host`] (`TokioHost`, the `qfs serve` runtime host) — that
//! is the SERVER side and unrelated; this module (plural `hosts`) is the CLIENT-of-hosts registry.
//!
//! Selectors + metadata only — no token is stored here (the login records the host, not a
//! credential; the session token is the future protocol ticket's concern).

use qfs_cmd::HostAction;
use rusqlite::{Connection, OptionalExtension};

/// The implicit embedded host name — present without a login, refuses logout.
pub(crate) const LOCAL_HOST: &str = "local";

/// One row of the client-side hosts registry (selectors + metadata only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRow {
    /// The host name a mount references.
    pub name: String,
    /// The base URL for a remote host; `None` for `local`.
    pub url: Option<String>,
    /// `local` (embedded) or `remote` (a self-hosted or managed qfs server).
    pub kind: String,
}

/// The injected host launcher. Returns the process exit code (`0` ok, `1` on a structured error).
#[must_use]
pub fn run_host(action: &HostAction) -> i32 {
    match run_inner(action) {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("qfs: error: {e}");
            1
        }
    }
}

fn run_inner(action: &HostAction) -> Result<String, String> {
    let conn = open_system_conn()?;
    match action {
        HostAction::List => list_hosts(&conn),
        HostAction::Login { url } => login(&conn, url),
        HostAction::Logout { name } => logout(&conn, name),
    }
}

/// Open the migrated System DB and yield its owned connection (the `hosts` table lives here).
fn open_system_conn() -> Result<Connection, String> {
    let sys = crate::store::open_system_db()
        .map_err(|e| format!("opening the system database: {e}"))?
        .ok_or("cannot determine the system database path (set HOME or XDG_CONFIG_HOME)")?;
    Ok(sys.into_db().into_connection())
}

/// `qfs host list` — every recorded host (always includes the implicit `local`).
fn list_hosts(conn: &Connection) -> Result<String, String> {
    let rows = db_list_hosts(conn)?;
    let mut out = String::new();
    for r in &rows {
        match (&r.url, r.name.as_str()) {
            (_, LOCAL_HOST) => out.push_str(&format!("{}\t(implicit, embedded)\n", r.name)),
            (Some(url), _) => out.push_str(&format!("{}\t{url}\t({})\n", r.name, r.kind)),
            (None, _) => out.push_str(&format!("{}\t({})\n", r.name, r.kind)),
        }
    }
    out.push_str(&format!("{} host(s)", rows.len()));
    Ok(out)
}

/// `qfs host login <url>` — RECORD a remote host (no network I/O — the protocol is deferred). The
/// host name is derived from the URL host component so a mount can reference it.
fn login(conn: &Connection, url: &str) -> Result<String, String> {
    let name = host_name_from_url(url)?;
    if name == LOCAL_HOST {
        return Err("`local` is the implicit embedded host — it needs no login".into());
    }
    db_upsert_host(conn, &name, Some(url), "remote")?;
    Ok(format!(
        "recorded host `{name}` ({url}). Note: remote hosts are not yet executable — `qfs host \
         login` records the host so a mount can reference it; the remote session protocol is on \
         the roadmap (ADR 0008 §6). Nothing was sent over the network."
    ))
}

/// `qfs host logout <name>` — forget a recorded host. `local` is refused.
fn logout(conn: &Connection, name: &str) -> Result<String, String> {
    if name == LOCAL_HOST {
        return Err("`local` is the implicit embedded host — it cannot be removed".into());
    }
    let n = db_remove_host(conn, name)?;
    Ok(if n == 0 {
        format!("no host `{name}` was recorded (nothing to remove)")
    } else {
        format!("forgot host `{name}`")
    })
}

/// Derive a stable host NAME from a login URL — its host component (`https://qfs.cloud/…` →
/// `qfs.cloud`). A bare name (no scheme, no slash) is taken verbatim so `qfs host login qfs.cloud`
/// works. Refuses an empty result. NO network I/O.
fn host_name_from_url(url: &str) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("empty host URL".into());
    }
    // Strip a scheme, then take up to the first `/`, `?`, or `:` (port).
    let after_scheme = trimmed.split("://").last().unwrap_or(trimmed);
    let host = after_scheme
        .split(['/', '?', ':'])
        .next()
        .unwrap_or(after_scheme);
    if host.is_empty() {
        return Err(format!("could not derive a host name from `{url}`"));
    }
    Ok(host.to_string())
}

// ---- Store I/O (free functions over a System-DB connection) --------------------------------

/// Validate a `--host` name for a mount (used by `qfs connect --host <name>`). Fail-closed by the
/// current implementation state (ADR 0008 §6 — the remote protocol is deferred):
///
/// - `None` / `local` → OK (the implicit embedded host).
/// - a KNOWN remote → refused: remote hosts are **not yet executable**. `host login` records a
///   host so a mount can reference it once the session protocol lands, but binding to it now would
///   be a mount that cannot resolve. Fail closed, never a silent non-functional mount.
/// - an UNKNOWN name → refused with the record-it-first remedy.
///
/// # Errors
/// A structured, actionable error for a remote or unknown host.
pub fn require_known_host(host: Option<&str>) -> Result<(), String> {
    let name = host.unwrap_or(LOCAL_HOST);
    if name == LOCAL_HOST {
        return Ok(());
    }
    let conn = open_system_conn()?;
    if db_get_host(&conn, name)?.is_some() {
        Err(format!(
            "host `{name}` is a remote host, which is not yet executable — `qfs host login` \
             records it for when the remote session protocol lands (ADR 0008 §6), but a mount \
             cannot bind to it yet. Omit --host to use the implicit local host."
        ))
    } else {
        Err(format!(
            "unknown host `{name}` — record it first with `qfs host login <url>` (or omit \
             --host to use the implicit local host)"
        ))
    }
}

/// UPSERT a host record (last-writer-wins on `name`).
fn db_upsert_host(
    conn: &Connection,
    name: &str,
    url: Option<&str>,
    kind: &str,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO hosts (name, url, kind) VALUES (?1, ?2, ?3) \
         ON CONFLICT(name) DO UPDATE SET url = excluded.url, kind = excluded.kind",
        rusqlite::params![name, url, kind],
    )
    .map_err(|e| format!("recording the host: {e}"))?;
    Ok(())
}

/// Remove a host record; returns rows removed (0 if absent — idempotent).
fn db_remove_host(conn: &Connection, name: &str) -> Result<u64, String> {
    let n = conn
        .execute("DELETE FROM hosts WHERE name = ?1", rusqlite::params![name])
        .map_err(|e| format!("removing the host: {e}"))?;
    Ok(n as u64)
}

/// List every host, `local` first then by name.
fn db_list_hosts(conn: &Connection) -> Result<Vec<HostRow>, String> {
    let mut stmt = conn
        .prepare("SELECT name, url, kind FROM hosts ORDER BY (name = 'local') DESC, name")
        .map_err(|e| format!("listing hosts: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(HostRow {
                name: r.get(0)?,
                url: r.get(1)?,
                kind: r.get(2)?,
            })
        })
        .map_err(|e| format!("listing hosts: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("listing hosts: {e}"))?;
    Ok(rows)
}

/// Read one host by name (`None` if unrecorded).
fn db_get_host(conn: &Connection, name: &str) -> Result<Option<HostRow>, String> {
    conn.query_row(
        "SELECT name, url, kind FROM hosts WHERE name = ?1",
        rusqlite::params![name],
        |r| {
            Ok(HostRow {
                name: r.get(0)?,
                url: r.get(1)?,
                kind: r.get(2)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("reading the host: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_store::{migrate, Db, MemorySource, SYSTEM_MIGRATIONS};

    fn migrated() -> Connection {
        let mut db = Db::open(&MemorySource).unwrap();
        migrate(&mut db, SYSTEM_MIGRATIONS).unwrap();
        db.into_connection()
    }

    /// The implicit `local` host is always present, logging in a remote records it (no network),
    /// and logout forgets it — the round-trip.
    #[test]
    fn hosts_round_trip_with_local_always_present() {
        let conn = migrated();
        // `local` is seeded by the migration.
        assert!(db_get_host(&conn, LOCAL_HOST).unwrap().is_some());
        assert_eq!(db_list_hosts(&conn).unwrap().len(), 1);

        login(&conn, "https://qfs.cloud/team").unwrap();
        let row = db_get_host(&conn, "qfs.cloud").unwrap().unwrap();
        assert_eq!(row.url.as_deref(), Some("https://qfs.cloud/team"));
        assert_eq!(row.kind, "remote");
        // A duplicate login upserts, not duplicates.
        login(&conn, "https://qfs.cloud/other").unwrap();
        assert_eq!(db_list_hosts(&conn).unwrap().len(), 2);

        assert!(logout(&conn, "qfs.cloud").unwrap().contains("forgot"));
        assert!(db_get_host(&conn, "qfs.cloud").unwrap().is_none());
        // Logout of an absent host is idempotent.
        assert!(logout(&conn, "qfs.cloud")
            .unwrap()
            .contains("nothing to remove"));
    }

    /// `local` refuses both login and logout (it is the implicit host).
    #[test]
    fn local_is_protected() {
        let conn = migrated();
        assert!(login(&conn, "local").is_err());
        assert!(logout(&conn, LOCAL_HOST).is_err());
        assert!(db_get_host(&conn, LOCAL_HOST).unwrap().is_some());
    }

    /// The connect-time host gate, fail-closed by current state: `local` OK, a known remote is
    /// "not yet executable", an unknown name says record-it-first. (Exercised over `db_get_host`
    /// directly — `require_known_host` opens the real System DB, covered by the release smoke.)
    #[test]
    fn host_gate_classification() {
        let conn = migrated();
        // local: always present, always allowed.
        assert!(db_get_host(&conn, LOCAL_HOST).unwrap().is_some());
        // a recorded remote exists (so the gate would say "not yet executable")...
        login(&conn, "https://qfs.cloud").unwrap();
        assert!(db_get_host(&conn, "qfs.cloud").unwrap().is_some());
        // ...and an unrecorded name does not (so the gate would say "record it first").
        assert!(db_get_host(&conn, "nope.example").unwrap().is_none());
    }

    /// A host name is derived from the URL's host component; a bare name is taken verbatim.
    #[test]
    fn host_name_derivation() {
        assert_eq!(
            host_name_from_url("https://qfs.cloud/x").unwrap(),
            "qfs.cloud"
        );
        assert_eq!(
            host_name_from_url("http://mycorp.example:8443").unwrap(),
            "mycorp.example"
        );
        assert_eq!(host_name_from_url("qfs.cloud").unwrap(), "qfs.cloud");
        assert!(host_name_from_url("   ").is_err());
    }
}
