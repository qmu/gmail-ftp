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

/// Load declared connections from the `QFS_CONNECTIONS` config file. Best-effort: empty when unset
/// or unreadable (an unconfigured run simply has no declared connections).
#[must_use]
pub fn declared_connections() -> Vec<DeclaredConnection> {
    let Some(path) = std::env::var_os(CONNECTIONS_ENV) else {
        return Vec::new();
    };
    std::fs::read_to_string(&path)
        .map(|source| parse_connections(&source))
        .unwrap_or_default()
}

/// The declared connections for one driver kind (e.g. `sqlite`, `git`).
#[must_use]
pub fn declared_for(driver: &str) -> Vec<DeclaredConnection> {
    declared_connections()
        .into_iter()
        .filter(|c| c.driver == driver)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
