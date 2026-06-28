//! t42: the binary's System-DB composition root — open the per-host [`qfs_store::SystemDb`] and
//! apply embedded migrations on start.
//!
//! The binary is the ONLY crate that resolves a real DB path (decision F + the dep-direction guard:
//! `qfs-store` is a leaf; nothing in the spine names a file). Opening the DB and running migrations
//! is **start-time infrastructure**, NOT a qfs effect-plan — it never goes through preview/commit;
//! it is the substrate later `/sys/*` writes (t53) preview and commit *over*.
//!
//! t42 wires this WITHOUT routing any existing command through it: the file vault still backs
//! secrets until t43. So this is best-effort — a host with no resolvable config home, or a transient
//! open error, must not block the CLI.

use std::path::PathBuf;

use qfs_store::{FileSource, ProjectDb, StoreError, SystemDb};

/// Resolve the default System DB path.
///
/// **OPEN PRODUCT DECISION (flagged in t42 for the reviewer, not baked in):** the System DB sits
/// alongside the existing credential vault under `~/.config/qfs/` — the current
/// `qfs_secrets::default_credentials_path()` XDG/HOME convention — rather than a new
/// `~/.local/share/qfs/`. t43/t53 may revisit once a real surface uses it; until then this is the
/// least-surprising home (one `qfs` config dir, not two).
#[must_use]
pub fn default_system_db_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("qfs").join("system.db"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("qfs")
                .join("system.db")
        })
}

/// Open the System DB at the default path and apply migrations (idempotent; a second start is a
/// no-op). Returns:
/// - `Ok(Some(db))` — opened + migrated;
/// - `Ok(None)` — no resolvable config home (HOME/XDG unset), so the binary runs without it (no
///   command routes through the System DB yet, t42);
/// - `Err(_)` — a real open/migration failure the caller may log (still non-fatal in t42).
pub fn open_system_db() -> Result<Option<SystemDb>, StoreError> {
    let Some(path) = default_system_db_path() else {
        return Ok(None);
    };
    Ok(Some(SystemDb::open(&FileSource::new(path))?))
}

/// Resolve the default Project DB path (`$XDG_CONFIG_HOME/qfs/project.db`, falling back to
/// `~/.config/qfs/project.db`), mirroring [`default_system_db_path`] so both DBs share the one `qfs`
/// config dir.
///
/// **OPEN PRODUCT DECISION (flagged for the reviewer, t43 — not baked in):** today this is a SINGLE
/// default `project.db` (one project per host). The roadmap's §4.2 model is one Project DB *per
/// project*; the unresolved question is whether each project gets its OWN `project-<slug>.db` file
/// (file-per-project) or whether projects become rows inside one DB keyed by a project id
/// (rows-in-System, decision F's tenant→DB route). Until a real multi-project surface lands (t44+),
/// the binary opens one default `project.db`; revisit the file layout then.
#[must_use]
pub fn default_project_db_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("qfs").join("project.db"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("qfs")
                .join("project.db")
        })
}

/// Open the Project DB at the default path and apply migrations (idempotent). Returns:
/// - `Ok(Some(db))` — opened + migrated (the t43 secret-store schema is now present);
/// - `Ok(None)` — no resolvable config home (HOME/XDG unset);
/// - `Err(_)` — a real open/migration failure.
///
/// The binary moves the migrated connection into the SQLite credential backend via the t42 seam
/// (`into_db().into_connection()` → `SqliteSecrets::open_or_init`).
pub fn open_project_db() -> Result<Option<ProjectDb>, StoreError> {
    let Some(path) = default_project_db_path() else {
        return Ok(None);
    };
    Ok(Some(ProjectDb::open(&FileSource::new(path))?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    // We mutate process env (HOME/XDG) which is global; serialize via a mutex so the cases that
    // read it don't race each other under the test harness's threads.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn xdg_config_home_takes_precedence() {
        let _g = ENV_LOCK.lock().unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", "/x/cfg");
        assert_eq!(
            default_system_db_path(),
            Some(PathBuf::from("/x/cfg/qfs/system.db"))
        );
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }

    #[test]
    fn project_db_path_follows_xdg_then_home() {
        let _g = ENV_LOCK.lock().unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", "/x/cfg");
        assert_eq!(
            default_project_db_path(),
            Some(PathBuf::from("/x/cfg/qfs/project.db"))
        );
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }

    #[test]
    fn open_project_db_creates_and_migrates_the_secret_store() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        let proj = open_project_db().unwrap().expect("config home resolves");
        // All project migrations applied (skeleton + t43 secret store + t54 consent ledger).
        assert_eq!(qfs_store::applied_migrations(proj.db()).unwrap().len(), 3);
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }

    #[test]
    fn open_system_db_creates_and_remigrates_idempotently() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        // First open creates + migrates (v1 skeleton + v2 audit chain t76 + v3 identity t45 + v4
        // sessions t46 + v5 oauth keys t48 + v6 oauth flow clients/codes t49 + v7 /sys/policies t53 +
        // v8 invites/memberships t55 + v9 oidc providers t56 + v10 /sys/settings t59).
        let sys = open_system_db().unwrap().expect("config home resolves");
        assert_eq!(qfs_store::applied_migrations(sys.db()).unwrap().len(), 10);
        drop(sys);
        // Second open is a verified no-op (still the same applied migrations).
        let sys2 = open_system_db().unwrap().expect("config home resolves");
        assert_eq!(qfs_store::applied_migrations(sys2.db()).unwrap().len(), 10);
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }
}
