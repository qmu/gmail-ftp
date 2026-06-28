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

use std::sync::Arc;

use qfs_core::MountRegistry;

/// Build the describe-only [`MountRegistry`]. Every driver is constructed cred-free; only the
/// introspective (pure) half is ever invoked by `qfs describe`. Registration failures are
/// impossible here (distinct mounts), but a duplicate would be dropped silently rather than
/// panicking — the registry stays a best-effort describe surface.
#[must_use]
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
