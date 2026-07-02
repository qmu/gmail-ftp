//! **Cloud-mount enumeration** (ADR 0008 §4 — mount-bound accounts): the one place the three
//! live registries (plan/describe, read, apply) learn which **connect-created cloud mounts**
//! exist and which account each is bound to.
//!
//! A cloud mount is a `path_binding` FULL-connect row whose `driver_id` names a cloud **kind**
//! (gmail/gdrive/ga/github/slack/s3/r2/cf). The mount's path becomes the registered driver id
//! (via [`crate::mount_adapter`]), and the mount's `account` — never a process-global selection —
//! names the credential it binds. Aliases are not cloud mounts (they reuse their target's
//! registration); local kinds (sql/git/fs/…) keep their own config-gated registration.

use crate::mount_adapter::MountRemap;
use crate::path_binding::PathBindingRow;

/// One connect-created cloud mount: the user path, the cloud kind, and the bound account label
/// (a Google email, a token label). Selectors + metadata only — never a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudMount {
    /// The user-defined mount path, e.g. `/mail2`.
    pub path: String,
    /// The cloud kind (the `path_binding.driver_id`), e.g. `gmail`.
    pub kind: String,
    /// The account label the mount binds (ADR 0008: the mount carries the account). `None` is a
    /// mount with no usable account — every live registration fails closed on it.
    pub account: Option<String>,
}

impl CloudMount {
    /// The remap between this mount's path and its kind's canonical driver id — the shared
    /// prefix arithmetic all three facet wrappers of the mount use. `None` when the path is
    /// malformed (fail closed; the mount is skipped, never a panic).
    #[must_use]
    pub fn remap(&self) -> Option<MountRemap> {
        MountRemap::new(&self.path, canonical_id(&self.kind)?).ok()
    }
}

/// The canonical **plan identity** a cloud kind's driver registers under when self-mounted —
/// the id the driver's own path parser speaks (`/{id}/…` reconstruction), so the inner side of
/// every mount remap. `None` for a non-cloud kind (sql/git/…, which are not per-account mounts).
#[must_use]
pub fn canonical_id(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "gmail" => "mail",
        "gdrive" => "drive",
        "ga" | "google-analytics" => "ga",
        "github" => "github",
        "slack" => "slack",
        "s3" => "s3",
        "r2" => "r2",
        "cf" => "cf",
        _ => return None,
    })
}

/// Project the cloud mounts out of the defined-path bindings: FULL connects only (aliases reuse
/// their target's registration), cloud kinds only. Pure — unit-testable without a DB.
#[must_use]
pub fn cloud_mounts_from(bindings: &[PathBindingRow]) -> Vec<CloudMount> {
    bindings
        .iter()
        .filter(|b| b.alias_of.is_none())
        .filter_map(|b| {
            let kind = b.driver_id.clone()?;
            canonical_id(&kind)?;
            Some(CloudMount {
                path: b.path.clone(),
                kind,
                account: b.account.clone(),
            })
        })
        .collect()
}

/// Load the cloud mounts from the Project DB `path_binding` registry (best-effort, cred-free):
/// an absent/unreadable Project DB is an empty list — nothing is pre-mounted, so every cloud
/// registry facet fails closed exactly like the CONNECT model demands.
#[must_use]
pub fn load_cloud_mounts() -> Vec<CloudMount> {
    match crate::store::open_project_db() {
        Ok(Some(proj)) => {
            let conn = proj.into_db().into_connection();
            crate::path_binding::db_list_bindings(&conn)
                .map(|rows| cloud_mounts_from(&rows))
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        path: &str,
        driver: Option<&str>,
        alias: Option<&str>,
        account: Option<&str>,
    ) -> PathBindingRow {
        PathBindingRow {
            path: path.to_string(),
            driver_id: driver.map(str::to_string),
            at_locator: None,
            secret_ref: None,
            alias_of: alias.map(str::to_string),
            host: "local".to_string(),
            account: account.map(str::to_string),
            created_at: String::new(),
        }
    }

    #[test]
    fn projects_full_connect_cloud_rows_only() {
        let bindings = vec![
            row("/mail", Some("gmail"), None, Some("work@example.com")),
            row("/mail2", Some("gmail"), None, Some("home@example.com")),
            // An alias reuses its target's registration — not a cloud mount.
            row("/m", None, Some("/mail"), None),
            // A local kind is not a per-account cloud mount.
            row("/work/orders", Some("postgres"), None, None),
            // A cloud mount with no account still enumerates (registration fails closed on it).
            row("/gh", Some("github"), None, None),
        ];
        let mounts = cloud_mounts_from(&bindings);
        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[0].path, "/mail");
        assert_eq!(mounts[0].account.as_deref(), Some("work@example.com"));
        assert_eq!(mounts[1].path, "/mail2");
        assert_eq!(mounts[2].path, "/gh");
        assert_eq!(mounts[2].account, None);
    }

    #[test]
    fn remap_pairs_the_mount_path_with_the_kinds_canonical_id() {
        let m = CloudMount {
            path: "/mail2".into(),
            kind: "gmail".into(),
            account: None,
        };
        let remap = m.remap().expect("valid remap");
        assert_eq!(remap.outer_id().as_str(), "mail2");
        assert_eq!(remap.path_in("/mail2/inbox"), "/mail/inbox");
        // The renamed analytics driver keys its namespace by ID (`ga`), not its mount.
        let ga = CloudMount {
            path: "/analytics".into(),
            kind: "ga".into(),
            account: None,
        };
        let remap = ga.remap().expect("valid remap");
        assert_eq!(remap.path_in("/analytics/prop"), "/ga/prop");
    }
}
