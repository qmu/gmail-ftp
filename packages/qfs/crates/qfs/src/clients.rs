//! The shared **live-client builders** for the credentialed networked drivers (github, slack) —
//! the single source of truth both the commit applier ([`crate::commit::live_registry`]) and the
//! read facets ([`crate::shell::run_engine_and_reads`]) construct their real clients through.
//!
//! ## Why a shared builder (the ticket's single-source ask)
//! A networked read (`FROM /github/.../pulls`) and a networked write (`INSERT INTO /github/...`)
//! hit the SAME API behind the SAME credential + the SAME t54/t81/t80 bind gates. Before this
//! module the commit path built its `RestGitHubClient`/`RestSlackClient` inline; the read facet
//! would have had to duplicate that construction (and, dangerously, the bind-gate decision). These
//! builders factor it into one place: each returns the credentialed `Arc<dyn …Client>` ready to be
//! wrapped either in a driver's apply leg (commit) or a `ReadDriver` adapter (read), so the two
//! funnels can never disagree about which connection binds or how the token is resolved.
//!
//! ## Fail closed (no creds / no consent ⇒ no client)
//! Each builder returns `None` whenever the credential cannot be resolved into a key
//! ([`crate::commit::networked_credential`]) OR the t54 cloud bind gate refuses the selected
//! connection ([`crate::commit::cloud_bind_allowed`] — not signed in, or no recorded consent). A
//! `None` leaves the driver UNREGISTERED on both the apply and the read side, so an unconfigured
//! `/github`/`/slack` statement fails honestly ("no driver / no source") rather than acting — or
//! reading — without authorization. The SECRET itself is never read here: the real client resolves
//! the PAT / bot token lazily at request-build time, so a missing/locked credential surfaces as a
//! clear per-request auth error, never a panic at construction (§3 purity + redaction preserved).

use std::sync::Arc;

use qfs_driver_github::{GitHubClient, RestGitHubClient};
use qfs_driver_slack::{BodyErrorRule, RestSlackClient, SlackClient};

/// Build the live, credentialed GitHub client when the operator is configured AND the t54 cloud
/// bind gate passes for the selected connection; else `None` (fail closed). The returned
/// `Arc<dyn GitHubClient>` is the SAME construction the commit applier and the read facet share.
#[must_use]
pub(crate) fn live_github_client() -> Option<Arc<dyn GitHubClient>> {
    let (store, cred) = crate::commit::networked_credential("github")?;
    if !crate::commit::cloud_bind_allowed("github", cred.connection.as_str()) {
        return None;
    }
    Some(Arc::new(RestGitHubClient::new(
        crate::transport::github_transport(),
        store,
        cred,
    )))
}

/// Build the live, credentialed Slack client (body-error rule ON, the Slack setting) when the
/// operator is configured AND the t54 cloud bind gate passes; else `None` (fail closed). The
/// returned `Arc<dyn SlackClient>` is shared between the commit applier and the read facet.
#[must_use]
pub(crate) fn live_slack_client() -> Option<Arc<dyn SlackClient>> {
    let (store, cred) = crate::commit::networked_credential("slack")?;
    if !crate::commit::cloud_bind_allowed("slack", cred.connection.as_str()) {
        return None;
    }
    Some(Arc::new(RestSlackClient::new(
        crate::transport::slack_transport(),
        store,
        cred,
        BodyErrorRule::On,
    )))
}
