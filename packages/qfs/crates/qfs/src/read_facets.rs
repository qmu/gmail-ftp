//! The **networked read adapters** — the read counterparts of [`crate::shell::LocalReadDriver`],
//! hosted in the `qfs` binary crate. Each wraps a credentialed driver client behind the async
//! [`qfs_exec::ReadDriver`] seam so a `FROM /github/.../pulls` (or `FROM /slack/<ws>/users`)
//! executes through the read executor, the same way `LocalReadDriver` services `FROM /local/...`.
//!
//! ## Why the adapters live in the BINARY (the same CO-t29-4 topology as the local read facet)
//! `ReadDriver` is a `qfs-exec` type, and the driver crates must stay OFF `qfs-exec` (the
//! dep-direction confinement guard: a `qfs-runtime` consumer must be a leaf). qfs-exec cannot
//! depend on the driver crates either. The binary is the one node that is BOTH an allowlisted
//! runtime consumer AND a terminal sink, so the adapter that bridges the driver's pure
//! `read_rows` into the async `ReadDriver` lives here — exactly like `LocalReadDriver`,
//! `SysReadDriver`, and `ClaudeReadDriver`. The path→plan→fetch→decode logic itself lives INSIDE
//! each driver crate (`qfs_driver_github::read_rows` / `qfs_driver_slack::read_rows`), so this
//! adapter only owns the async boundary + the error mapping; it never re-derives the read logic.
//!
//! ## Fail closed (the ticket's honesty bar)
//! The adapter is registered (by [`crate::shell::run_engine_and_reads`]) only when the shared
//! [`crate::clients`] builder yields a credentialed client — i.e. the operator is configured and
//! the t54 cloud bind gate passed. When it is registered but the credential cannot be resolved at
//! request time (no token, locked store), the underlying client returns a structured auth error
//! and this adapter surfaces it as a [`CfsError`] carrying the driver's stable secret-free `code`
//! — **never** an empty `RowBatch`, never a panic. The SECRET never crosses this seam (the driver
//! errors are secret-free by construction; the planted-canary tests in each driver assert this).

use std::sync::Arc;

use qfs_core::{CfsError, RowBatch};
use qfs_driver_github::GitHubClient;
use qfs_driver_slack::SlackClient;
use qfs_exec::ReadDriver;
use qfs_pushdown::ScanNode;

/// The GitHub read facet: adapts [`qfs_driver_github::read_rows`] (the pure-then-I/O
/// path→plan→fetch→decode composition) to qfs-exec's async [`ReadDriver`] seam. Owns the
/// credentialed [`GitHubClient`] the shared builder constructed; no vendor type crosses the seam —
/// only the owned [`ScanNode`] in and the owned [`RowBatch`] out.
pub struct GitHubReadDriver {
    client: Arc<dyn GitHubClient>,
}

impl GitHubReadDriver {
    /// Build the read adapter over an injected credentialed client.
    #[must_use]
    pub fn new(client: Arc<dyn GitHubClient>) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ReadDriver for GitHubReadDriver {
    async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError> {
        // The ScanNode carries the full addressed VFS path (t28 pushdown threading) + the pushed
        // predicate; the driver's read_rows owns the parse → ReadPlan → list → decode composition.
        let predicate = scan.pushed.filter.as_ref();
        qfs_driver_github::read_rows(self.client.as_ref(), &scan.path, predicate).map_err(|e| {
            // A networked read failure (auth/transport/API/decode/path) becomes a structured,
            // secret-free CfsError carrying the driver's stable code — never empty rows.
            CfsError::InvalidPath {
                path: scan.path.clone(),
                reason: e.code(),
            }
        })
    }
}

/// The Slack read facet: adapts [`qfs_driver_slack::read_rows`] to qfs-exec's async [`ReadDriver`]
/// seam. The structural twin of [`GitHubReadDriver`], over the credentialed [`SlackClient`].
pub struct SlackReadDriver {
    client: Arc<dyn SlackClient>,
}

impl SlackReadDriver {
    /// Build the read adapter over an injected credentialed client.
    #[must_use]
    pub fn new(client: Arc<dyn SlackClient>) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ReadDriver for SlackReadDriver {
    async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError> {
        let predicate = scan.pushed.filter.as_ref();
        qfs_driver_slack::read_rows(self.client.as_ref(), &scan.path, predicate).map_err(|e| {
            CfsError::InvalidPath {
                path: scan.path.clone(),
                reason: e.code(),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    //! Hermetic adapter tests — no socket, no real credential. The happy path drives the adapter
    //! over each driver's in-memory MOCK client (proving the async seam threads the path + predicate
    //! through `read_rows` and returns the decoded rows). The fail-closed path drives the adapter
    //! over the REAL `RestGitHubClient` backed by an EMPTY secret store, proving a credential-less
    //! networked read returns a structured auth error — not empty rows, not a panic.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use qfs_driver_github::{MockGitHubClient, RestGitHubClient, TransportError};
    use qfs_driver_http::{HttpRequest, HttpResponse};
    use qfs_driver_slack::MockSlackClient;
    use qfs_pushdown::PushedQuery;
    use qfs_secrets::{ConnectionId, CredentialKey, InMemoryStore, Secrets};
    use qfs_types::{Schema, Value};

    /// A `ScanNode` over `path` with no pushed query (the bare collection read tests use).
    fn scan_for(path: &str) -> ScanNode {
        ScanNode {
            source: qfs_pushdown::SourceId::new("test"),
            path: path.to_string(),
            pushed: PushedQuery::default(),
            schema: Schema::new(Vec::new()),
        }
    }

    /// A transport that must never be called — the fail-closed test proves auth fails BEFORE any
    /// wire exchange, so reaching `send` is itself the failure.
    struct NeverCalled;
    impl qfs_driver_github::HttpTransport for NeverCalled {
        fn send(&self, _req: &HttpRequest) -> Result<HttpResponse, TransportError> {
            panic!("the transport must not be reached: auth must fail closed first");
        }
    }

    #[tokio::test]
    async fn github_adapter_reads_a_collection_through_the_mock_client() {
        let client = MockGitHubClient::new().with_list(serde_json::json!([
            { "number": 7, "title": "t", "state": "open", "user": { "login": "octocat" },
              "head": { "ref": "f", "sha": "s" }, "base": { "ref": "main" }, "merged": false },
        ]));
        let driver = GitHubReadDriver::new(Arc::new(client));
        let batch = driver
            .scan(&scan_for("/github/octocat/hello/pulls"))
            .await
            .unwrap();
        assert_eq!(batch.rows.len(), 1);
        assert_eq!(batch.rows[0].values[0], Value::Int(7));
    }

    #[tokio::test]
    async fn slack_adapter_reads_the_users_directory_through_the_mock_client() {
        let client = MockSlackClient::new().with_list(serde_json::json!({
            "members": [{ "id": "U1", "name": "alice", "real_name": "Alice", "is_bot": false,
                          "deleted": false }]
        }));
        let driver = SlackReadDriver::new(Arc::new(client));
        let batch = driver.scan(&scan_for("/slack/acme/users")).await.unwrap();
        assert_eq!(batch.rows.len(), 1);
        assert_eq!(batch.rows[0].values[0], Value::Text("U1".to_string()));
    }

    #[tokio::test]
    async fn github_read_without_credentials_fails_closed_with_an_auth_error() {
        // A registered read facet whose credential cannot be resolved (empty store) returns a
        // structured auth error at request time — NOT an empty batch, NOT a panic. The transport is
        // never reached (auth resolution precedes any wire exchange).
        let store: Arc<dyn Secrets> = Arc::new(InMemoryStore::new());
        let cred = CredentialKey::new(
            qfs_secrets::DriverId("github".to_string()),
            ConnectionId::new("default").unwrap(),
        );
        let client = RestGitHubClient::new(Arc::new(NeverCalled), store, cred);
        let driver = GitHubReadDriver::new(Arc::new(client));
        let err = driver
            .scan(&scan_for("/github/octocat/hello/pulls"))
            .await
            .unwrap_err();
        // The structured CfsError carries the driver's stable auth code as its reason (secret-free).
        match err {
            CfsError::InvalidPath { reason, .. } => assert_eq!(reason, "github_auth"),
            other => panic!("expected a structured auth path error, got {other:?}"),
        }
    }
}
