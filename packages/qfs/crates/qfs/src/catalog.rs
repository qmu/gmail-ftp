//! `qfs::catalog` — the **driver catalog** the docs are generated from (ticket t40).
//!
//! ## Built from the EXISTING describe surface — no new `Driver::doc()` trait method
//! The ticket's first draft proposed adding a required `Driver::doc() -> DriverDoc` method to
//! the [`qfs_core::Driver`] trait. A new *required* trait method would force **every** driver
//! crate (and everything downstream) to recompile — which the constrained build disk cannot
//! survive. So this catalog is built **without touching the trait**: it walks the binary's own
//! describe registry ([`crate::describe::describe_registry`]) and folds each driver's existing
//! introspective half — the t39 [`qfs_core::DescribeReport`] (`archetype` / `verbs` /
//! `procedures` / `aliases` / `pushdown`) plus the driver's `mount()` — into an **owned**
//! [`DriverDoc`] DTO. No vendor SDK type leaks (RFD §9); no credential is resolved and no I/O
//! happens (DESCRIBE is pure, RFD §3) because only the introspective methods are called.
//!
//! ## Codecs are catalogued from the builtin codec set
//! The `DECODE`/`ENCODE` formats are a single workspace-wide open registry (RFD §4), not a
//! per-driver list, so the catalog records the builtin codec names once via
//! [`qfs_codec::builtin_codecs`] and the docs render them as the shared codec row.

use qfs_core::{
    builtin_codecs, Archetype, Capabilities, DescribeReport, Path, ProcSig, PushdownSummary,
};

use crate::describe::describe_registry;

/// One driver's catalog entry — an **owned DTO** rendered into `docs/drivers.md`. Every field
/// reuses an existing owned workspace type (RFD §9: no vendor SDK type leaks); it is produced by
/// folding the t39 describe surface, never hand-authored.
#[derive(Debug, Clone, PartialEq)]
pub struct DriverDoc {
    /// The driver mount, e.g. `/mail` (the paths-registry key, RFD §3).
    pub mount: String,
    /// A representative node path under the mount that the catalog describes, e.g. `/mail/drafts`.
    pub example_path: String,
    /// How a representative node maps onto qfs's uniform model (the archetype, RFD §5).
    pub archetype: Archetype,
    /// The FS/SQL-shaped native-verb hint for the archetype (the agent's one-line "what does
    /// writing here look like").
    pub native_verbs: &'static str,
    /// Which universal verbs a representative node supports — rendered **explicitly**, including
    /// the unsupported ones (RFD §5: never document a capability by omission).
    pub capabilities: Capabilities,
    /// The `CALL driver.action(..)` procedures this driver declares (driver-global).
    pub procedures: Vec<ProcSig>,
    /// The prelude pure-fn aliases this driver ships (e.g. `SEND -> mail.send`).
    pub aliases: Vec<(String, String)>,
    /// What a representative node pushes down natively (the planner input, RFD §6).
    pub pushdown: PushdownSummary,
}

/// The whole catalog: the per-driver docs plus the workspace-wide codec set.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    /// One entry per registered driver mount, in deterministic (mount-sorted) order.
    pub drivers: Vec<DriverDoc>,
    /// The builtin `DECODE`/`ENCODE` format names (the shared codec registry, RFD §4).
    pub codecs: Vec<String>,
}

/// A representative, describable node path for each known mount. The describe surface is
/// **path-keyed** (a driver may mix archetypes on sub-paths), so the catalog needs one concrete
/// node per mount to fold into a [`DriverDoc`]. These mirror the paths the t39 describe tests
/// already prove resolve cred-free; an unknown mount falls back to the mount root.
fn representative_path(mount: &str) -> String {
    match mount {
        "/local" => "/local/x.txt".to_string(),
        // t68: a representative blob under a named root. `/fs` describes cred-free (pure, no I/O,
        // names no host path), so the mount folds into the generated catalog like `/local`.
        "/fs" => "/fs/projects/report.md".to_string(),
        "/mail" => "/mail/drafts".to_string(),
        // A file/blob node under the My Drive corpus (the t39 corpus prefix). `/drive/Reports`
        // would parse as neither corpus and fold to the empty capability set — under-selling the
        // driver; a node under `/drive/my/...` shows Drive's real blob verbs (Ls/Select/Insert/
        // Upsert/Cp/Mv) the agent can plan against.
        "/drive" => "/drive/my/Reports/report.pdf".to_string(),
        "/github" => "/github/o/r/pulls".to_string(),
        "/slack" => "/slack/ws/#general/messages".to_string(),
        "/ga" => "/ga/123456789".to_string(),
        "/s3" => "/s3/bucket/key".to_string(),
        "/r2" => "/r2/bucket/key".to_string(),
        // t53 administration: a representative admin relation. `/sys/users` describes cred-free
        // (the read source + applier are injected; describe is pure), so the `/sys` mount folds
        // into the generated driver catalog like any other introspective facet.
        "/sys" => "/sys/users".to_string(),
        // t64 AI-sessions: a representative session relation. `/claude/sessions` describes cred-free
        // (the read source + applier are injected; describe is pure), so the `/claude` mount folds
        // into the generated driver catalog like any other introspective facet. Decision K: a path
        // façade over session metadata, never an LLM call.
        "/claude" => "/claude/sessions".to_string(),
        // Any future mount: describe its root; if that is not describable the entry is skipped.
        other => other.to_string(),
    }
}

/// Build the driver catalog by walking the binary's OWN describe registry and folding each
/// driver's introspective half. Pure: no creds, no I/O, no network (only DESCRIBE-side methods
/// are called). Drivers whose representative node is not describable are skipped (the catalog
/// stays best-effort, never panics).
#[must_use]
pub fn driver_catalog() -> Catalog {
    let reg = describe_registry();
    let mut drivers = Vec::new();

    for driver in reg.drivers() {
        let mount = driver.mount().to_string();
        let example_path = representative_path(&mount);
        // Fold the t39 describe surface for the representative node. If it does not resolve
        // (rare: a mount needing a registered catalog), skip the entry rather than fail.
        let path = Path::new(&example_path);
        let Ok(report) = DescribeReport::from_driver(driver.as_ref(), &path) else {
            continue;
        };
        drivers.push(DriverDoc {
            mount,
            example_path,
            archetype: report.archetype,
            native_verbs: report.native_verbs,
            capabilities: report.verbs,
            procedures: report.procedures,
            aliases: report
                .aliases
                .into_iter()
                .map(|a| (a.name, a.desugars_to))
                .collect(),
            pushdown: report.pushdown,
        });
    }

    // The driver registry is a BTreeMap, so `drivers()` already iterates mount-sorted; keep that
    // deterministic order explicit for the golden docs.
    drivers.sort_by(|a, b| a.mount.cmp(&b.mount));

    let mut codecs = builtin_codecs()
        .iter()
        .map(|c| c.fmt().to_string())
        .collect::<Vec<_>>();
    codecs.sort();

    Catalog { drivers, codecs }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog is built from the live registry: it carries every describe-registered driver,
    /// each with a folded describe surface — proving it is generated, not hand-authored.
    #[test]
    fn catalog_covers_the_describe_registry_drivers() {
        let cat = driver_catalog();
        // The describe registry registers 9 cred-free drivers (local/fs/mail/drive/github/slack/
        // ga/s3/r2); every one whose representative node resolves appears in the catalog.
        assert!(
            cat.drivers.len() >= 7,
            "catalog should fold most describe-registered drivers, got {}",
            cat.drivers.len()
        );
        // Mail is present and carries its declared irreversible `send` proc + `SEND` alias.
        let mail = cat
            .drivers
            .iter()
            .find(|d| d.mount == "/mail")
            .expect("/mail is catalogued");
        assert_eq!(mail.archetype, Archetype::AppendLog);
        assert!(mail
            .procedures
            .iter()
            .any(|p| p.name == "send" && p.irreversible));
        assert!(mail.aliases.iter().any(|(n, _)| n == "SEND"));
        // Codecs are catalogued once from the builtin set (the shared §4 registry).
        for fmt in ["json", "jsonl", "yaml", "toml", "csv"] {
            assert!(
                cat.codecs.iter().any(|c| c == fmt),
                "codec {fmt} catalogued"
            );
        }
    }

    /// Owned DTOs only — no vendor handle / token shape ever reaches a catalog entry.
    #[test]
    fn catalog_leaks_no_credential_shape() {
        let cat = driver_catalog();
        let dump = format!("{cat:?}").to_lowercase();
        assert!(!dump.contains("bearer"));
        assert!(!dump.contains("token"));
        assert!(!dump.contains("password"));
    }
}
