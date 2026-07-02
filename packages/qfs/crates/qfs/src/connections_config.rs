//! Load `CREATE CONNECTION` declarations from a `connections.qfs` config file and expose them as
//! [`DeclaredConnection`] records the driver registries (`crate::sql`, `crate::git`, …) build their
//! mounts from — the in-language replacement for the `QFS_SQL_*` / `QFS_GIT_*` env-var alias
//! convention (the connection epic `20260630004100`).
//!
//! The *parse* lives in `qfs-core` ([`qfs_core::ddl::connections`]) because the dep-direction guard
//! pins this binary off the parser spine; here we own only the file/env I/O over that parse. This is
//! deliberately lighter than the server boot: a connection is a mount-config concern needed even for
//! a plain `qfs run`, so declarations are read directly rather than committed through `qfs serve`.
//! Best-effort: empty when unconfigured or unreadable, so a typo never crashes a read.

pub use qfs_core::ddl::connections::{parse_connections, DeclaredConnection};

/// The env var naming the connections config file: `QFS_CONNECTIONS=/path/to/connections.qfs`.
pub const CONNECTIONS_ENV: &str = "QFS_CONNECTIONS";

/// Load declared connections from the connections config: `QFS_CONNECTIONS` if set, else the default
/// path (`$XDG_CONFIG_HOME/qfs/connections.qfs`, falling back to `~/.config/qfs/connections.qfs`).
/// Best-effort: empty when no file is found or it is unreadable (an unconfigured run simply has no
/// declared connections). Loaded the same way for `qfs run`, `qfs serve`, and `qfs job` — they all
/// build their mounts through the driver registries that consult this.
#[must_use]
pub fn declared_connections() -> Vec<DeclaredConnection> {
    let Some(path) = config_path() else {
        return Vec::new();
    };
    std::fs::read_to_string(&path)
        .map(|source| parse_connections(&source))
        .unwrap_or_default()
}

/// Resolve the connections config path: the explicit `QFS_CONNECTIONS` override, else the default
/// `<config-home>/qfs/connections.qfs` **only when it exists** (so a non-existent default is silent).
fn config_path() -> Option<std::path::PathBuf> {
    if let Some(explicit) = std::env::var_os(CONNECTIONS_ENV) {
        return Some(std::path::PathBuf::from(explicit));
    }
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })?;
    let default = config_home.join("qfs").join("connections.qfs");
    default.exists().then_some(default)
}

/// The declared connections for one driver kind (e.g. `sqlite`, `git`).
#[must_use]
pub fn declared_for(driver: &str) -> Vec<DeclaredConnection> {
    declared_connections()
        .into_iter()
        .filter(|c| c.driver == driver)
        .collect()
}

/// Emit, **once per process**, a deprecation note when a `QFS_SQL_*` / `QFS_GIT_*` env-var
/// connection is used — nudging the operator toward an in-language declaration. The env vars stay a
/// working fallback; this is the warned-deprecation signal, not a removal.
pub fn warn_env_var_deprecation_once() {
    use std::sync::Once;
    static WARNED: Once = Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "qfs: warning: QFS_SQL_* / QFS_GIT_* connection env vars are deprecated. Declare \
             connections with `CREATE CONNECTION <name> DRIVER <driver> AT '<locator>'` in a \
             connections.qfs (point at it with QFS_CONNECTIONS=<file>, or \
             ~/.config/qfs/connections.qfs). Run `qfs connect --import-env` to print the \
             equivalent declarations."
        );
    });
}

/// Build the `CREATE CONNECTION` declarations equivalent to the current `QFS_SQL_*` / `QFS_GIT_*`
/// env vars (the `qfs connect --import-env` migration). Reads only the non-secret locators; a
/// SQLite/git connection carries no secret, so the output is paste-ready and secret-free.
#[must_use]
pub fn import_env_declarations() -> String {
    let mut lines: Vec<String> = Vec::new();
    for (key, value) in std::env::vars() {
        let mapped = key
            .strip_prefix("QFS_SQL_")
            .map(|name| (name, "sqlite"))
            .or_else(|| key.strip_prefix("QFS_GIT_").map(|name| (name, "git")));
        let Some((name, driver)) = mapped else {
            continue;
        };
        if name.is_empty() || value.is_empty() {
            continue;
        }
        // Escape a single quote in the locator so the rendered AT '…' stays well-formed.
        let locator = value.replace('\'', "\\'");
        lines.push(format!(
            "CREATE CONNECTION {} DRIVER {driver} AT '{locator}';",
            name.to_ascii_lowercase(),
        ));
    }
    lines.sort();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_env_renders_declarations_for_sql_and_git_vars() {
        std::env::set_var("QFS_SQL_SHOP", "/data/shop.db");
        std::env::set_var("QFS_GIT_APP", "/srv/app.git");
        let out = import_env_declarations();
        assert!(out.contains("CREATE CONNECTION shop DRIVER sqlite AT '/data/shop.db';"));
        assert!(out.contains("CREATE CONNECTION app DRIVER git AT '/srv/app.git';"));
        std::env::remove_var("QFS_SQL_SHOP");
        std::env::remove_var("QFS_GIT_APP");
    }

    #[test]
    fn declared_for_filters_by_driver_over_parsed_declarations() {
        // The parse logic is tested in qfs-core; here we cover the driver filter shape.
        let conns = parse_connections(
            "CREATE CONNECTION a DRIVER sqlite AT '/a.db';\n\
             CREATE CONNECTION b DRIVER git AT '/b.git';",
        );
        let sqlite: Vec<_> = conns.into_iter().filter(|c| c.driver == "sqlite").collect();
        assert_eq!(sqlite.len(), 1);
        assert_eq!(sqlite[0].name, "a");
    }
}
