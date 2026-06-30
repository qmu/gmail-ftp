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

use qfs_core::{CfsError, Column, ColumnType, Name, Path, Row, RowBatch, Schema, Value};
use qfs_driver_ga::{GaDriver, QuerySpec as GaQuerySpec};
use qfs_driver_gdrive::GDriveClient;
use qfs_driver_git::{blobfs, relational, GitDriver, GitNode, GitPath};
use qfs_driver_github::GitHubClient;
use qfs_driver_gmail::GmailClient;
use qfs_driver_objstore::{object_listing_schema, ObjDriver, ObjNode};
use qfs_driver_slack::SlackClient;
use qfs_driver_sql::{QuerySpec, SqlDriver};
use qfs_exec::ReadDriver;
use qfs_pushdown::{PushedQuery, ScanNode};

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

/// The SQL read facet: adapts [`SqlDriver::execute_query`] (compile the pushed
/// projection/`WHERE`/`ORDER BY`/`LIMIT` into ONE native parameterized `SELECT`, run it, return the
/// rows + the residual predicate SQL could not faithfully render) to qfs-exec's async [`ReadDriver`]
/// seam. This is the "filters push **into** the database" path — the native `SELECT` does the
/// pushable work; the residual (e.g. a `LIKE`/regex the dialect can't express exactly) is re-filtered
/// locally via [`qfs_exec::apply_residual`] so the returned rows are exactly the pushed query's
/// result before the engine runs the remaining cross-source residual. Unlike the cloud facets this
/// is hermetic against a SQLite file — no network, no credential.
pub struct SqlReadDriver {
    driver: Arc<SqlDriver>,
}

impl SqlReadDriver {
    /// Build the read adapter over a live [`SqlDriver`] (its connection registry already
    /// introspected the catalog).
    #[must_use]
    pub fn new(driver: Arc<SqlDriver>) -> Self {
        Self { driver }
    }
}

/// Translate the planner's owned [`PushedQuery`] into the SQL compiler's [`QuerySpec`] — the pushed
/// projection (column names), `WHERE` predicate, `ORDER BY`, and `LIMIT` the native `SELECT` runs.
fn query_spec_from_pushed(pushed: &PushedQuery) -> QuerySpec {
    let projection = pushed.project.as_ref().map_or_else(Vec::new, |cols| {
        cols.iter().map(|c| c.as_str().to_string()).collect()
    });
    let mut spec = QuerySpec::new(projection);
    if let Some(predicate) = &pushed.filter {
        spec = spec.with_predicate(predicate.clone());
    }
    for order in &pushed.order {
        spec = spec.order_by(order.column.as_str(), order.descending);
    }
    if let Some(limit) = pushed.limit {
        spec = spec.with_limit(i64::try_from(limit).unwrap_or(i64::MAX));
    }
    spec
}

/// Narrow `batch` to exactly the columns in `cols` (the pushed projection), in that order — the
/// facet's job once a projection was pushed (no local Project op remains to do it). Columns absent
/// from the batch are skipped (the SELECT is the source of truth; a missing column never panics).
fn project_batch(batch: &RowBatch, cols: &[Name]) -> RowBatch {
    let picks: Vec<usize> = cols
        .iter()
        .filter_map(|name| {
            batch
                .schema
                .columns
                .iter()
                .position(|c| c.name.as_str() == name.as_str())
        })
        .collect();
    let schema = Schema::new(
        picks
            .iter()
            .map(|i| batch.schema.columns[*i].clone())
            .collect(),
    );
    let rows = batch
        .rows
        .iter()
        .map(|r| {
            Row::new(
                picks
                    .iter()
                    .map(|i| r.values.get(*i).cloned().unwrap_or(Value::Null))
                    .collect(),
            )
        })
        .collect();
    RowBatch::new(schema, rows)
}

/// The object-storage read facet (t-203070): adapts the in-house [`ObjDriver`]'s `ls` (native S3
/// `list_objects_v2` with prefix/delimiter pushdown) and `get` (streaming download) to qfs-exec's
/// async [`ReadDriver`] seam. A `/<scheme>/<bucket>` (or prefix) node LISTS its objects into the
/// canonical [`object_listing_schema`]; a concrete `/<scheme>/<bucket>/<key>` node DOWNLOADS that
/// object's bytes into a one-row `content` batch (mirroring the local/git/drive content reads, so
/// `… |> decode <fmt>` works). The live SigV4 backend is injected (built by `crate::commit`); no
/// credential or vendor type crosses this seam.
pub struct ObjReadDriver {
    driver: Arc<ObjDriver>,
}

impl ObjReadDriver {
    /// Build the read adapter over a live [`ObjDriver`] (its [`qfs_driver_objstore::ObjRegistry`]
    /// carries the SigV4 [`qfs_driver_objstore::HttpBackend`] for the configured bucket).
    #[must_use]
    pub fn new(driver: Arc<ObjDriver>) -> Self {
        Self { driver }
    }
}

/// The single-row object-content batch: `key` + the raw bytes under the well-known `content` column
/// (the name the engine's `DECODE` reads, matching the local/git/drive content reads).
fn object_content_batch(key: &str, bytes: Vec<u8>) -> RowBatch {
    let schema = Schema::new(vec![
        Column::new("key", ColumnType::Text, false),
        Column::new("content", ColumnType::Bytes, true),
    ]);
    let row = Row::new(vec![Value::Text(key.to_string()), Value::Bytes(bytes)]);
    RowBatch::new(schema, vec![row])
}

/// The blocking object-storage read: parse the node, then LIST (bucket) or DOWNLOAD (object). The
/// SigV4 backend's reqwest transport drives its own runtime via `block_on`, so the caller runs this
/// off the async executor's thread (see [`ObjReadDriver::scan`]).
fn obj_scan(
    driver: &ObjDriver,
    path_str: &str,
    filter: Option<&qfs_core::Predicate>,
    limit: Option<u64>,
) -> Result<RowBatch, CfsError> {
    let invalid = |reason: &'static str| CfsError::InvalidPath {
        path: path_str.to_string(),
        reason,
    };
    let path = Path::new(path_str);
    match ObjNode::parse(&path).map_err(|e| invalid(e.code()))? {
        // A concrete object key downloads its content (one `content` row).
        ObjNode::Object { key, .. } => {
            let bytes = driver
                .get(&path, None)
                .map_err(|e| invalid(e.code()))?
                .into_bytes();
            Ok(object_content_batch(&key, bytes))
        }
        // A bucket/prefix lists objects (native prefix/delimiter pushdown + truthful residual).
        ObjNode::Bucket { .. } => {
            let pushdown = ObjDriver::plan_ls(filter, None);
            let (page, residual) = driver
                .ls(&path, &pushdown, None)
                .map_err(|e| invalid(e.code()))?;
            let batch = RowBatch::new(object_listing_schema(), page.to_rows());
            let mut batch = match residual {
                Some(predicate) => qfs_exec::apply_residual(batch, &predicate),
                None => batch,
            };
            if let Some(limit) = limit {
                batch
                    .rows
                    .truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            }
            Ok(batch)
        }
        // The mount root (no bucket) is not a readable node.
        _ => Err(invalid("objstore_root_not_readable")),
    }
}

#[async_trait::async_trait]
impl ReadDriver for ObjReadDriver {
    async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError> {
        let path_str = scan.path.clone();
        let filter = scan.pushed.filter.clone();
        let limit = scan.pushed.limit;
        let err_path = scan.path.clone();
        // The SigV4 backend's reqwest transport drives its OWN runtime via `block_on`; running it
        // inside the async read executor would nest runtimes (a panic). Execute the blocking read on
        // a DEDICATED OS thread that has no tokio context. (Every cloud read facet shares this
        // transport — objstore is the first wired to run live; the others need the same when used
        // live, relevant to the EPIC `20260630203030` live verification.)
        let joined = std::thread::scope(|s| {
            s.spawn(|| obj_scan(&self.driver, &path_str, filter.as_ref(), limit))
                .join()
        });
        match joined {
            Ok(result) => result,
            Err(_) => Err(CfsError::InvalidPath {
                path: err_path,
                reason: "objstore_read_panicked",
            }),
        }
    }
}

#[async_trait::async_trait]
impl ReadDriver for SqlReadDriver {
    async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError> {
        let spec = query_spec_from_pushed(&scan.pushed);
        let path = Path::new(&scan.path);
        // `execute_query` returns the rows, the truthful residual, AND the schema the SELECT actually
        // produced: the (residual-expanded) projection when a projection pushed, else the full catalog
        // schema. The driver KEEPS every column the residual reads, so the residual re-filter below is
        // always sound; the projection narrow (last) drops the extra columns again.
        let (rows, residual, out_schema) =
            self.driver
                .execute_query(&path, &spec)
                .map_err(|e| CfsError::InvalidPath {
                    path: scan.path.clone(),
                    reason: e.code(),
                })?;
        let batch = RowBatch::new(out_schema, rows);
        // The driver applied the faithfully-renderable part natively; re-filter the residual locally
        // so the rows are exactly the pushed query's result (over-returning on the pushed predicate
        // is NOT corrected by the engine — the pushed work is the driver's responsibility).
        let mut batch = match residual {
            Some(predicate) => qfs_exec::apply_residual(batch, &predicate),
            None => batch,
        };
        // Enforce the pushed `LIMIT` AFTER the residual re-filter. `compile` emits a native SQL
        // `LIMIT` only when nothing is residual (else it would under-fetch); when a residual remained
        // the backend returned the unlimited set, so the cap is applied here. A no-op when the
        // backend already limited (it returned ≤ limit rows).
        if let Some(limit) = scan.pushed.limit {
            batch
                .rows
                .truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        // A pushed PROJECTION leaves NO local Project op in the plan, so the facet must deliver
        // exactly the requested columns. The SELECT may have over-fetched residual columns (above), so
        // narrow to the requested projection LAST — after the residual re-filter has used them.
        if let Some(project) = &scan.pushed.project {
            batch = project_batch(&batch, project);
        }
        Ok(batch)
    }
}

/// The history-node fetch cap: relational reads (`commits`/`changes`/`blame`) need a concrete
/// bound, so the facet fetches up to this many rows and lets the engine residual apply the real
/// `WHERE`/`LIMIT` (the git pushdown profile declares nothing pushable — correctness over
/// optimization, mirroring the SQL facet). Generous enough for ordinary histories.
const GIT_READ_CAP: usize = 10_000;

/// An honest read facet for a cloud source whose reads fundamentally need a live, authenticated
/// account (mail / drive / analytics / object stores). It always fails with a clear, actionable
/// reason — "connect your account" — instead of leaving the source UNREGISTERED, which surfaces the
/// internal-sounding `unknown_source` ("no read driver registered") to a fresh user (the t5 honesty
/// fix). When the real networked read facet lands (t6/t7) it is registered over this one for a
/// credentialed operator; until then every reader gets the same actionable nudge, never empty rows.
pub struct ConnectAccountReadDriver {
    reason: &'static str,
}

impl ConnectAccountReadDriver {
    /// Build the facet with a service-specific actionable `reason` (a stable `&'static str` that
    /// the executor renders as the error message — secret-free, machine-legible).
    #[must_use]
    pub fn new(reason: &'static str) -> Self {
        Self { reason }
    }
}

#[async_trait::async_trait]
impl ReadDriver for ConnectAccountReadDriver {
    async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError> {
        Err(CfsError::InvalidPath {
            path: scan.path.clone(),
            reason: self.reason,
        })
    }
}

/// The Gmail read facet (t7): adapts [`qfs_driver_gmail::read_rows`] (parse the `/mail/<label>` or
/// `/mail/drafts` path → search the label's message ids → fetch each into the canonical
/// `MailMessage` rows) to the async [`ReadDriver`] seam — the structural twin of [`GitHubReadDriver`]
/// over the credentialed [`GmailClient`]. Network: the composition is proven hermetically by
/// driver-gmail's mock-client test; a real read needs a live OAuth account (registered over the
/// connect-account fallback only when the operator is connected and the bind gate passes).
pub struct GmailReadDriver {
    client: Arc<dyn GmailClient>,
}

impl GmailReadDriver {
    /// Build the read adapter over an injected credentialed [`GmailClient`].
    #[must_use]
    pub fn new(client: Arc<dyn GmailClient>) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ReadDriver for GmailReadDriver {
    async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError> {
        let predicate = scan.pushed.filter.as_ref();
        qfs_driver_gmail::read_rows(
            self.client.as_ref(),
            &scan.path,
            predicate,
            scan.pushed.limit,
        )
        .map_err(|e| CfsError::InvalidPath {
            path: scan.path.clone(),
            reason: e.code(),
        })
    }
}

/// The Google Drive read facet: adapts [`qfs_driver_gdrive::read_rows`] (parse the `/drive/...` path
/// → walk folder names to Drive file ids → list the resolved folder's children into `FileMeta`
/// rows) to the async [`ReadDriver`] seam — the structural twin of [`GmailReadDriver`] over the
/// credentialed [`GDriveClient`]. Hermetically proven by driver-gdrive's mock-client walk test; a
/// real read needs a live OAuth account (registered over the connect-account fallback only when the
/// operator is connected and the bind gate passes).
pub struct DriveReadDriver {
    client: Arc<dyn GDriveClient>,
}

impl DriveReadDriver {
    /// Build the read adapter over an injected credentialed [`GDriveClient`].
    #[must_use]
    pub fn new(client: Arc<dyn GDriveClient>) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ReadDriver for DriveReadDriver {
    async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError> {
        let predicate = scan.pushed.filter.as_ref();
        qfs_driver_gdrive::read_rows(self.client.as_ref(), &scan.path, predicate).map_err(|e| {
            CfsError::InvalidPath {
                path: scan.path.clone(),
                reason: e.code(),
            }
        })
    }
}

/// The Google Analytics read facet: adapts [`GaDriver::execute_query`] (parse the `/ga/<property>`
/// path → fetch the property catalog → compile the pushed projection/`WHERE`/`ORDER BY`/`LIMIT` into
/// one GA4 `runReport` → project the response onto its typed schema) to qfs-exec's async
/// [`ReadDriver`] seam. GA is the two-step (catalog + report) analog of the SQL facet; the residual
/// (a `contains`/regex GA cannot express exactly) is re-filtered locally. Hermetically proven by
/// driver-ga's mock-client tests; a real read needs a live OAuth account.
pub struct GaReadDriver {
    driver: Arc<GaDriver>,
}

impl GaReadDriver {
    /// Build the read adapter over a live [`GaDriver`] (its injected client carries the auth).
    #[must_use]
    pub fn new(driver: Arc<GaDriver>) -> Self {
        Self { driver }
    }
}

/// Translate the planner's owned [`PushedQuery`] into the GA compiler's [`GaQuerySpec`] — the pushed
/// projection (dimension/metric names), `WHERE`, `ORDER BY`, and `LIMIT` the `runReport` runs.
fn ga_query_spec_from_pushed(pushed: &PushedQuery) -> GaQuerySpec {
    let projection = pushed.project.as_ref().map_or_else(Vec::new, |cols| {
        cols.iter().map(|c| c.as_str().to_string()).collect()
    });
    let mut spec = GaQuerySpec::new(projection);
    if let Some(predicate) = &pushed.filter {
        spec = spec.with_predicate(predicate.clone());
    }
    for order in &pushed.order {
        spec = spec.order_by(order.column.as_str(), order.descending);
    }
    if let Some(limit) = pushed.limit {
        spec = spec.with_limit(i64::try_from(limit).unwrap_or(i64::MAX));
    }
    spec
}

#[async_trait::async_trait]
impl ReadDriver for GaReadDriver {
    async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError> {
        let spec = ga_query_spec_from_pushed(&scan.pushed);
        let path = Path::new(&scan.path);
        let (rows, schema, residual) =
            self.driver
                .execute_query(&path, &spec)
                .map_err(|e| CfsError::InvalidPath {
                    path: scan.path.clone(),
                    reason: e.code(),
                })?;
        let batch = RowBatch::new(schema, rows);
        let mut batch = match residual {
            Some(predicate) => qfs_exec::apply_residual(batch, &predicate),
            None => batch,
        };
        // Enforce the pushed `LIMIT` after the residual re-filter (GA's `compile` emits a native
        // `limit` only when nothing is residual, mirroring the SQL facet — see its `scan`).
        if let Some(limit) = scan.pushed.limit {
            batch
                .rows
                .truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        Ok(batch)
    }
}

/// Build a [`RowBatch`] from typed DTO rows via their `schema()` + `to_row()` (the git relational
/// nodes: `commits`/`changes`/`refs`/`tags`/`reflog`/`blame`).
fn dto_batch<T>(schema: Schema, rows: &[T], to_row: impl Fn(&T) -> Row) -> RowBatch {
    RowBatch::new(schema, rows.iter().map(to_row).collect())
}

/// The git read facet: adapts the in-house object reader (`relational` history nodes +
/// `blobfs` tree/blob reads, ADR-0003 — no `gix`) to qfs-exec's async [`ReadDriver`] seam. Reads a
/// repository's commits / changes / refs / tags / reflog / blame and versioned-tree listings at any
/// `@<ref>` coordinate. Hermetic against a local `.git` (no network); the engine residual applies
/// `WHERE`/projection/`LIMIT`.
pub struct GitReadDriver {
    driver: Arc<GitDriver>,
}

impl GitReadDriver {
    /// Build the read adapter over a live [`GitDriver`] (its [`qfs_driver_git::RepoResolver`] maps
    /// each `/git/<repo>` segment to a resolved repository).
    #[must_use]
    pub fn new(driver: Arc<GitDriver>) -> Self {
        Self { driver }
    }
}

#[async_trait::async_trait]
impl ReadDriver for GitReadDriver {
    async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError> {
        let invalid = |reason: &'static str| CfsError::InvalidPath {
            path: scan.path.clone(),
            reason,
        };
        let gp = GitPath::parse(&scan.path).map_err(|e| invalid(e.code()))?;
        let repo = self
            .driver
            .repos()
            .repo(&gp.repo)
            .map_err(|e| invalid(e.code()))?;
        let r = gp.reference.as_str();
        use qfs_driver_git::{BlameRow, ChangeRow, CommitRow, RefRow, ReflogRow};
        let batch = match &gp.node {
            GitNode::Commits => {
                let rows =
                    relational::commits(repo, r, GIT_READ_CAP).map_err(|e| invalid(e.code()))?;
                dto_batch(CommitRow::schema(), &rows, CommitRow::to_row)
            }
            GitNode::Changes => {
                let rows =
                    relational::changes(repo, r, GIT_READ_CAP).map_err(|e| invalid(e.code()))?;
                dto_batch(ChangeRow::schema(), &rows, ChangeRow::to_row)
            }
            GitNode::Refs => {
                let rows = relational::refs(repo);
                dto_batch(RefRow::schema(), &rows, RefRow::to_row)
            }
            GitNode::Tags => {
                let rows = relational::tags(repo);
                dto_batch(RefRow::schema(), &rows, RefRow::to_row)
            }
            GitNode::Reflog => {
                let rows = relational::reflog(repo, r);
                dto_batch(ReflogRow::schema(), &rows, ReflogRow::to_row)
            }
            GitNode::Blame { file } => {
                let rows =
                    blobfs::blame(repo, r, file, GIT_READ_CAP).map_err(|e| invalid(e.code()))?;
                dto_batch(BlameRow::schema(), &rows, BlameRow::to_row)
            }
            // A blob PATH addresses either a file (→ a `content` row) or a directory (→ the tree
            // listing); `blobfs::read` dispatches on which at read time (a file path resolved to a
            // blob node that `ls` alone rejected with `invalid_path`).
            GitNode::Blob { path } => blobfs::read(repo, r, path).map_err(|e| invalid(e.code()))?,
            GitNode::Root => blobfs::ls(repo, r, "").map_err(|e| invalid(e.code()))?,
            // GitNode is #[non_exhaustive]: a future node kind has no read wiring yet.
            _ => return Err(invalid("unsupported_git_node")),
        };
        Ok(batch)
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
    async fn sql_adapter_reads_a_table_and_pushes_where_into_the_database() {
        use qfs_types::{CmpOp, ColRef, Literal, Predicate};
        let (path, driver) = crate::sql::seeded_test_driver(
            "orders",
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, customer TEXT NOT NULL, total INTEGER NOT NULL);\
             INSERT INTO orders (customer,total) VALUES ('alice',50),('bob',150),('carol',250);",
        );
        let read = SqlReadDriver::new(Arc::new(driver));

        // Bare scan: every row, every column (the catalog-schema derivation path).
        let all = read.scan(&scan_for("/sql/orders/orders")).await.unwrap();
        assert_eq!(all.rows.len(), 3, "all seeded rows");
        assert_eq!(all.schema.columns.len(), 3, "id, customer, total");

        // Pushed WHERE total > 100: the native SELECT filters IN the database to bob + carol.
        let pushed = PushedQuery {
            filter: Some(Predicate::Cmp(
                ColRef::col("total"),
                CmpOp::Gt,
                Literal::Int(100),
            )),
            ..PushedQuery::default()
        };
        let scan = ScanNode {
            source: qfs_pushdown::SourceId::new("sql"),
            path: "/sql/orders/orders".to_string(),
            pushed,
            schema: Schema::new(Vec::new()),
        };
        let filtered = read.scan(&scan).await.unwrap();
        assert_eq!(filtered.rows.len(), 2, "WHERE total>100 keeps bob + carol");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn connect_account_facet_errors_actionably_not_unknown_source() {
        // A cloud source with no offline read returns an ACTIONABLE error (carrying the connect
        // hint) rather than leaving the source unregistered (the internal-sounding unknown_source).
        let facet =
            ConnectAccountReadDriver::new("connect a Google account to read mail — run signup");
        let err = facet.scan(&scan_for("/mail/inbox")).await.unwrap_err();
        match err {
            CfsError::InvalidPath { reason, path } => {
                assert!(
                    reason.contains("connect"),
                    "actionable connect hint: {reason}"
                );
                assert_eq!(path, "/mail/inbox");
            }
            other => panic!("expected an actionable InvalidPath, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn git_adapter_reads_commits_refs_and_tree_hermetically() {
        // An in-memory committed fixture (no git CLI, no network): one commit of one file, mirroring
        // the driver-git fixture's object construction. Proves the facet's per-node dispatch +
        // schema for commits / refs / tree listings.
        use qfs_driver_git::{
            serialize_tree, GitApplier, GitDriver, LooseObjectDb, ObjectDb, Repo, RepoResolver,
            Tree, TreeEntry,
        };
        use qfs_driver_git::{ObjectKind, Oid};
        let mut db = LooseObjectDb::new();
        let blob = db.insert_object(ObjectKind::Blob, b"hello\n");
        let tree = Tree {
            entries: vec![TreeEntry {
                mode: "100644".to_string(),
                name: "a.txt".to_string(),
                oid: blob,
            }],
        };
        let tree_oid = db.insert_object(ObjectKind::Tree, &serialize_tree(&tree));
        let commit_payload = format!(
            "tree {}\nauthor T <t@t> 1700000000 +0000\ncommitter T <t@t> 1700000000 +0000\n\nfirst\n",
            tree_oid.as_str()
        );
        let commit = db.insert_object(ObjectKind::Commit, commit_payload.as_bytes());
        let _ = Oid::parse(commit.as_str()); // commit oid is a valid sha (sanity)
        let mut repo = Repo::new(Arc::new(db) as Arc<dyn ObjectDb>);
        repo.set_ref("refs/heads/main", commit.clone());
        repo.set_ref("main", commit.clone());
        repo.set_ref("HEAD", commit);
        let resolver = RepoResolver::new().with_repo("r", repo);
        let driver = GitDriver::new(resolver, GitApplier::new());
        let read = GitReadDriver::new(Arc::new(driver));

        let commits = read.scan(&scan_for("/git/r/commits")).await.unwrap();
        assert_eq!(commits.rows.len(), 1, "one commit on HEAD");
        assert!(commits
            .schema
            .columns
            .iter()
            .any(|c| c.name.as_str() == "message"));

        let refs = read.scan(&scan_for("/git/r/refs")).await.unwrap();
        assert!(
            refs.rows
                .iter()
                .any(|row| matches!(&row.values[0], Value::Text(s) if s.contains("main"))),
            "refs lists the main branch"
        );

        let tree_listing = read.scan(&scan_for("/git/r/")).await.unwrap();
        assert!(
            tree_listing
                .rows
                .iter()
                .any(|row| matches!(&row.values[0], Value::Text(s) if s == "a.txt")),
            "the HEAD tree lists a.txt"
        );

        // t20260630203100: a blob FILE path reads its CONTENT (one row + a `content` column) rather
        // than erroring `invalid_path` like the listing-only `ls` did — mirroring a `/local/<file>`
        // read so `/git/r/a.txt |> decode <fmt>` has bytes to decode.
        let blob_content_col = |batch: &RowBatch| -> Vec<u8> {
            assert_eq!(
                batch.rows.len(),
                1,
                "a single blob reads as one content row"
            );
            let idx = batch
                .schema
                .columns
                .iter()
                .position(|c| c.name.as_str() == "content")
                .expect("a blob FILE read carries a `content` column");
            match &batch.rows[0].values[idx] {
                Value::Bytes(b) => b.clone(),
                other => panic!("the `content` column must be bytes, got {other:?}"),
            }
        };
        let file = read.scan(&scan_for("/git/r/a.txt")).await.unwrap();
        assert_eq!(
            blob_content_col(&file),
            b"hello\n",
            "the content column holds the blob's exact bytes"
        );
        // Combined with an explicit `@<ref>` coordinate (`/git/<repo>@<ref>/<file>`).
        let at_ref = read.scan(&scan_for("/git/r@main/a.txt")).await.unwrap();
        assert_eq!(
            blob_content_col(&at_ref),
            b"hello\n",
            "the same blob reads at an explicit @<ref>"
        );

        // A non-existent file path still fails closed with a structured invalid_path (not content).
        let missing = read.scan(&scan_for("/git/r/nope.txt")).await;
        assert!(
            matches!(missing, Err(CfsError::InvalidPath { .. })),
            "a missing blob path is a structured invalid_path, got {missing:?}"
        );
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
