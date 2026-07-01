//! The `qfs describe` composition root (ticket t39): builds the **describe-only** driver
//! [`MountRegistry`] that the `qfs describe <path>` subcommand consults, and injects it into
//! `qfs-cmd` via the [`qfs_cmd::DescribeProvider`].
//!
//! ## DESCRIBE is PURE — so the registry is cred-free
//! `DESCRIBE` reads only the **introspective** half of the [`qfs_core::Driver`] contract
//! (`describe` / `capabilities` / `procedures` / `prelude` / `pushdown`) — it never reaches
//! `Driver::applier`, so no credential is resolved, no socket is opened, no I/O happens (RFD §3
//! purity invariant). Each driver is therefore constructed with its **public, cred-free mock
//! client** (`Mock*Client` — explicitly "no socket, no credentials") or a registry carrying a
//! representative bucket on a cred-free `MockObjectBackend` (s3/r2), which the introspective
//! half reads for capabilities but never *applies*.
//!
//! ## Why the binary owns this
//! qfs-cmd must stay off the concrete `qfs-driver-*` crates (the dep_direction guard). The binary
//! is the allowlisted leaf that may carry those edges, so the registry is built here and injected
//! — exactly like the t28 shell launcher and the t32 serve launcher.
//!
//! ## Coverage (the LIGHT facet of the CO-t29-1 driver-registration carry-over)
//! Registered cred-free (no backend registration needed for describe): **local, fs, mail, drive,
//! github, slack, ga, s3, r2**. The t68 `/fs` driver describes over an EMPTY (deny-all) root
//! allowlist — its pure introspective half names no host path. **sql / git / cf** require a
//! registered connection-catalog / repo
//! / D1-catalog for describe to resolve a concrete node (a *registration* requirement, not a
//! credential one), so their describe is covered by the `qfs-skill` golden corpus instead — where
//! the harness builds the registry with a fixture catalog. This is the documented fallback.
//!
//! ## Driver TYPES + CONNECT-ed paths (t100040, the defined-path model)
//! This registry serves two purposes and so registers two things: the available driver **types**
//! (the cred-free catalogue above — the source of `docs/drivers.md` and the "what can I CONNECT?"
//! reference, always present) AND the developer's **CONNECT-ed defined paths** (each `path_binding`
//! row, mounted at its user path via [`register_defined_paths`]). A fresh binary with no bindings
//! shows only the type catalogue, so the generated docs are unaffected. The PLANNING registry
//! (`shell.rs`) registers ONLY the minimal system set + the bindings — nothing third-party is
//! pre-mounted for *use*; describe is the broader catalogue-plus-connections surface.

use std::sync::Arc;

use qfs_core::MountRegistry;

/// Build the describe-only [`MountRegistry`]. Every driver is constructed cred-free; only the
/// introspective (pure) half is ever invoked by `qfs describe`. Registration failures are
/// impossible here (distinct mounts), but a duplicate would be dropped silently rather than
/// panicking — the registry stays a best-effort describe surface.
#[must_use]
/// A cred-free Cloudflare registry carrying ONE representative D1 database / KV namespace / queue,
/// so `qfs describe /cf/d1/db` (and the t40 driver catalogue) surface `/cf`'s real verbs over the
/// public in-memory [`MockCfBackend`](qfs_driver_cf::MockCfBackend) — the same "representative
/// resource" shape the objstore describe uses for `/s3/bucket`. Never *applied* (describe reads only
/// the introspective half), so no credential and no I/O ever happens.
pub(crate) fn cred_free_cf_registry() -> qfs_driver_cf::CfRegistry {
    use qfs_driver_cf::{Catalog, CfRegistry, D1Database, MockCfBackend};
    CfRegistry::new()
        .with_d1(
            "db",
            D1Database::new(Arc::new(MockCfBackend::new()), Catalog::new(Vec::new())),
        )
        .with_kv("ns", Arc::new(MockCfBackend::new()))
        .with_queue("q", Arc::new(MockCfBackend::new()))
}

/// Build the **cred-free** driver instance for a canonical driver id — the planning + describe
/// facet of a CONNECT-ed third-party driver (t100040). Mock / empty clients only: the planner and
/// `describe` only ever touch the pure introspective half (`describe`/`capabilities`/`pushdown`),
/// never `applier`, so no credential is resolved and no I/O happens. The driver's `mount()` / `id()`
/// stay CANONICAL (e.g. `/mail`, `mail`); a binding mounts this instance at the user path via
/// `register_alias`, and the `/<id>/<sub>` reconstruction (t100030) keeps the driver's own parser
/// working under the user path.
///
/// Returns `None` for a driver id with no cred-free constructor here — notably `sql`/`git`, whose
/// MULTI-connection planning driver is built from the declared-connection seam (`crate::sql` /
/// `crate::git`), not a single mock instance; those keep their existing config-gated registration.
#[must_use]
pub(crate) fn cred_free_driver(driver_id: &str) -> Option<Arc<dyn qfs_core::Driver>> {
    let driver: Arc<dyn qfs_core::Driver> = match driver_id {
        "gmail" => Arc::new(qfs_driver_gmail::GmailDriver::new(Arc::new(
            qfs_driver_gmail::MockGmailClient::new(),
        ))),
        "gdrive" => Arc::new(qfs_driver_gdrive::GDriveDriver::new(Arc::new(
            qfs_driver_gdrive::MockDriveClient::default(),
        ))),
        "google-analytics" | "ga" => Arc::new(qfs_driver_ga::GaDriver::new(Arc::new(
            qfs_driver_ga::MockGaClient::default(),
        ))),
        "github" => Arc::new(qfs_driver_github::GitHubDriver::new(Arc::new(
            qfs_driver_github::MockGitHubClient::default(),
        ))),
        "slack" => Arc::new(qfs_driver_slack::SlackDriver::new(Arc::new(
            qfs_driver_slack::MockSlackClient::default(),
        ))),
        "s3" => Arc::new(qfs_driver_objstore::S3Driver::new(
            crate::objstore::planning_registry(qfs_driver_objstore::Scheme::S3),
        )),
        "r2" => Arc::new(qfs_driver_objstore::R2Driver::new(
            crate::objstore::planning_registry(qfs_driver_objstore::Scheme::R2),
        )),
        "cf" => Arc::new(qfs_driver_cf::CfDriver::new(cred_free_cf_registry())),
        "fs" => Arc::new(qfs_driver_fs::FsDriver::new(qfs_driver_fs::FsRoots::new())),
        "claude" => Arc::new(qfs_driver_claude::ClaudeDriver::new()),
        "rest" | "http" => {
            let json = qfs_core::CodecRegistry::with_builtins()
                .resolve("json")
                .ok()?;
            Arc::new(qfs_driver_http::RestDriver::new(
                qfs_driver_http::RestApiConfig::new("http://localhost", Vec::new()),
                json,
                Arc::new(qfs_driver_http::MockHttpClient::new()),
                Arc::new(qfs_secrets::InMemoryStore::new()),
            ))
        }
        _ => return None,
    };
    Some(driver)
}

/// Load the persisted defined-path bindings (best-effort, cred-free): the `path_binding` rows the
/// registration loop mounts. Returns an empty list when no Project DB resolves (a fresh binary has
/// no third-party mounts — nothing is pre-mounted, exactly the CONNECT model).
fn load_bindings() -> Vec<crate::path_binding::PathBindingRow> {
    match crate::store::open_project_db() {
        Ok(Some(proj)) => {
            let conn = proj.into_db().into_connection();
            crate::path_binding::db_list_bindings(&conn).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

/// Register every CONNECT-ed defined path from the project DB `path_binding` registry into `reg`
/// (t100040). A FULL connect mounts the cred-free driver for its `driver_id` at the user path; an
/// ALIAS mounts the SAME driver its target already resolves to. NOTHING is pre-mounted — a
/// third-party path resolves ONLY after a CONNECT. Fail-open per binding (an unknown driver id or a
/// dangling alias is skipped, never a panic) so one bad row cannot sink the whole registry.
pub(crate) fn register_defined_paths(reg: &mut MountRegistry) {
    let bindings = load_bindings();
    // Full connects first, so an alias's target mount already exists when the alias is processed.
    for b in bindings.iter().filter(|b| b.alias_of.is_none()) {
        if let Some(driver) = b.driver_id.as_deref().and_then(cred_free_driver) {
            let _ = reg.register_alias(&b.path, driver);
        }
    }
    for b in bindings.iter().filter(|b| b.alias_of.is_some()) {
        if let Some(target) = &b.alias_of {
            if let Some((driver, _)) = reg.resolve_path(target) {
                let _ = reg.register_alias(&b.path, driver);
            }
        }
    }
}

pub fn describe_registry() -> MountRegistry {
    let mut reg = MountRegistry::new();

    // Each driver's describe facet, constructed cred-free (mock client / empty registry). The
    // `register` result is intentionally ignored: distinct mounts never collide, and a describe
    // registry that dropped one entry is still a valid (if smaller) surface — never a panic.
    let drivers: Vec<Arc<dyn qfs_core::Driver>> = vec![
        // Blob: the reference local-FS driver (genuinely cred-free).
        Arc::new(qfs_driver_local::LocalFsDriver::new("/")),
        // Blob: the t68 first-class `/fs` driver. DESCRIBE is PURE — it names no host path and does
        // no I/O — so it describes cred-free over an EMPTY (deny-all) root allowlist; the live roots
        // are injected only on the apply registry (`commit.rs`). This is what makes `/fs` appear in
        // the generated `docs/drivers.md` without exposing any operator-configured directory.
        Arc::new(qfs_driver_fs::FsDriver::new(qfs_driver_fs::FsRoots::new())),
        // Append: Gmail (fixed describe; the MockGmailClient is never called by describe).
        Arc::new(qfs_driver_gmail::GmailDriver::new(Arc::new(
            qfs_driver_gmail::MockGmailClient::new(),
        ))),
        // Blob: Google Drive (fixed describe).
        Arc::new(qfs_driver_gdrive::GDriveDriver::new(Arc::new(
            qfs_driver_gdrive::MockDriveClient::default(),
        ))),
        // Object-graph: GitHub (path-keyed describe; no backend registration needed).
        Arc::new(qfs_driver_github::GitHubDriver::new(Arc::new(
            qfs_driver_github::MockGitHubClient::default(),
        ))),
        // Append/object: Slack (path-keyed describe).
        Arc::new(qfs_driver_slack::SlackDriver::new(Arc::new(
            qfs_driver_slack::MockSlackClient::default(),
        ))),
        // Relational: Google Analytics (path-keyed describe; schema filled at query time).
        Arc::new(qfs_driver_ga::GaDriver::new(Arc::new(
            qfs_driver_ga::MockGaClient::default(),
        ))),
        // Blob: S3 + R2 over a registry carrying ONE representative bucket (`bucket`), built on
        // the public, cred-free `MockObjectBackend` (in-memory fixtures — no creds, no socket, no
        // network). Per-node capabilities are gated on a *registered* bucket (a registration
        // requirement, not a credential one), so registering this one representative bucket lets
        // `qfs describe /s3/bucket/key` — and the t40 driver catalog — surface S3/R2's real blob
        // verbs instead of an empty set. The mock backend is never *applied* (DESCRIBE reads only
        // the introspective half), so no I/O ever happens.
        Arc::new(qfs_driver_objstore::S3Driver::new(
            qfs_driver_objstore::ObjRegistry::new().with_bucket(
                "bucket",
                qfs_driver_objstore::Bucket::new(Arc::new(
                    qfs_driver_objstore::MockObjectBackend::new(),
                )),
            ),
        )),
        Arc::new(qfs_driver_objstore::R2Driver::new(
            qfs_driver_objstore::ObjRegistry::new().with_bucket(
                "bucket",
                qfs_driver_objstore::Bucket::new(Arc::new(
                    qfs_driver_objstore::MockObjectBackend::new(),
                )),
            ),
        )),
        // t53 administration: the `/sys/*` admin surface. DESCRIBE is PURE — SysDriver owns NO
        // backend and NO creds (its read source + applier are injected from the binary), so it
        // describes `/sys/users`, `/sys/audit`, … cred-free, exactly like the other introspective
        // facets. This is what makes `/sys/*` appear in the generated `docs/drivers.md`.
        Arc::new(qfs_driver_sys::SysDriver::new()),
        // t64 AI-sessions (roadmap M7): the `/claude/...` session surface. DESCRIBE is PURE —
        // ClaudeDriver owns NO session source and NO creds (its read source + applier are injected
        // from the binary), so it describes `/claude/sessions` + `.../instructions` cred-free,
        // exactly like the other introspective facets. Decision K: this is a path façade over
        // session metadata + an append-log, NOT qfs calling an LLM. This is what makes `/claude/*`
        // appear in the generated `docs/drivers.md`.
        Arc::new(qfs_driver_claude::ClaudeDriver::new()),
        // Cloudflare (/cf) + the generic HTTP/REST (/rest) drivers: their PURE describe surfaces,
        // built cred-free (empty registry / placeholder config), so `qfs describe /cf` and
        // `qfs describe /rest` resolve and the t40 driver catalogue surfaces them — closing the
        // "exist in the code but aren't reachable as paths" gap. Live read/commit + per-resource
        // config (which D1/KV/queues; which REST resource maps) are the follow-up.
        Arc::new(qfs_driver_cf::CfDriver::new(cred_free_cf_registry())),
        // NOTE (t58): the `/directories/...` identity-directory driver is deliberately NOT
        // registered here. `/directories` is a RESERVED SCOPE REALM (decision P / §1.3 —
        // `RESERVED_REALMS`), not a driver-backed mount like `/sys`, so `MountRegistry::register`
        // governance rejects a `/directories` mount (proven by
        // `register_rejects_a_driver_mount_that_shadows_a_realm`). The t58 driver's PURE,
        // credential-free describe surface (`qfs_driver_directory::DirectoryDriver`) and its read
        // seam are instead consumed directly by the live `member_of` resolver in `src/directory.rs`;
        // routing a scope-realm `/directories/<provider>/groups` path THROUGH the driver for `qfs
        // describe` is the documented seam this read-first slice leaves open.
    ];

    for driver in drivers {
        // Ignore a (theoretically impossible) duplicate-mount error: the describe surface is
        // best-effort and must never panic.
        let _ = reg.register(driver);
    }
    // The generic HTTP/REST driver's cred-free describe mount (placeholder config + mock client +
    // empty in-memory secrets — never applied). Its codec is resolved from the builtin set; if that
    // somehow fails, /rest is simply absent rather than panicking (the best-effort describe rule).
    if let Ok(json) = qfs_core::CodecRegistry::with_builtins().resolve("json") {
        let _ = reg.register(Arc::new(qfs_driver_http::RestDriver::new(
            qfs_driver_http::RestApiConfig::new("http://localhost", Vec::new()),
            json,
            Arc::new(qfs_driver_http::MockHttpClient::new()),
            Arc::new(qfs_secrets::InMemoryStore::new()),
        )));
    }
    // t100040: ALSO surface the developer's CONNECT-ed defined paths, so `qfs describe` shows both
    // the available driver TYPES (the catalogue above, cred-free, the source of `docs/drivers.md`)
    // AND the paths this deployment has actually connected. A fresh binary with no bindings adds
    // nothing here (the catalogue stays the reference), so the generated docs are unaffected.
    register_defined_paths(&mut reg);
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The describe registry resolves the acceptance path `/mail/drafts` to its driver, and that
    /// driver's introspective half folds into a populated [`qfs_core::DescribeReport`] — no creds,
    /// no I/O (the mock client is never called).
    #[test]
    fn mail_drafts_describes_cred_free() {
        let reg = describe_registry();
        let (driver, _rest) = reg
            .resolve_path("/mail/drafts")
            .expect("/mail is registered in the describe registry");
        let report = qfs_core::DescribeReport::from_driver(
            driver.as_ref(),
            &qfs_core::Path::new("/mail/drafts"),
        )
        .expect("/mail/drafts is describable");
        assert_eq!(report.archetype, qfs_core::Archetype::AppendLog);
        assert!(!report.columns.is_empty(), "mail describe has columns");
        // The SEND prelude alias is surfaced for the agent (mail.send desugar target).
        assert!(report.aliases.iter().any(|a| a.name == "SEND"));
        // The irreversible mail.send procedure is declared.
        assert!(report
            .procedures
            .iter()
            .any(|p| p.name == "send" && p.irreversible));
        // Drafts supports INSERT + UPSERT (the retry-safe default).
        assert!(report.verbs.insert && report.verbs.upsert);
    }

    /// Every registered mount resolves and describes a representative node without creds — proving
    /// the registry is genuinely cred-free across all eight drivers.
    #[test]
    fn all_registered_mounts_describe_cred_free() {
        let reg = describe_registry();
        let cases = [
            ("/local/x.txt", qfs_core::Archetype::BlobNamespace),
            ("/mail/drafts", qfs_core::Archetype::AppendLog),
            ("/drive/Reports", qfs_core::Archetype::BlobNamespace),
            (
                "/github/o/r/pulls",
                qfs_core::Archetype::ObjectGraphWorkflow,
            ),
            (
                "/slack/ws/#general/messages",
                qfs_core::Archetype::AppendLog,
            ),
            ("/s3/bucket/key", qfs_core::Archetype::BlobNamespace),
            ("/r2/bucket/key", qfs_core::Archetype::BlobNamespace),
        ];
        for (path, want) in cases {
            let (driver, _rest) = reg
                .resolve_path(path)
                .unwrap_or_else(|| panic!("{path} resolves to a registered describe driver"));
            let report =
                qfs_core::DescribeReport::from_driver(driver.as_ref(), &qfs_core::Path::new(path))
                    .unwrap_or_else(|e| panic!("{path} should describe cred-free: {e:?}"));
            assert_eq!(report.archetype, want, "archetype mismatch for {path}");
        }
    }
}
