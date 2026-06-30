//! The three **open registries** (RFD-0001 §3): paths/mounts, functions +
//! procedures, and codecs. These are the governance mechanism — "a new backend =
//! zero keywords" — so they must sit in the shared engine glue that both the CLI and
//! the server resolve through.
//!
//! Each registry is generic over a **trait object** (`Arc<dyn Driver>` /
//! `Arc<dyn Codec>` / an owned `ProcSig`), not over concrete types
//! (fidelity guard G2): a new driver (E4) implements the trait and calls `register`
//! — it touches zero core types. All three share the identical `new` / `register` /
//! `resolve` shape and use `BTreeMap` for deterministic iteration (test stability).
//! Empty at E0; the unit tests prove empty / round-trip / duplicate / absent.

use std::collections::BTreeMap;
use std::sync::Arc;

use qfs_codec::Codec;
use qfs_driver::{CfsError, Driver, ProcSig};

/// The closed, reserved set of **scope realms** (decision P / RFD §1.3). A path names
/// three axes — *scope* (whose), *service* (what), *coordinate* (when) — and its root is
/// always exactly one of these realms. Four are **plural collections** that take a single
/// principal segment (`/members/alice/…`); two are **singletons** (`/me`, `/sys`).
///
/// The set is closed for the same reason the keyword set is frozen: it is what makes the
/// `(scope, service)` split decidable (the two §1.3 rules — reserved realm names + single
/// principal arity). Adding a realm is a deliberate governance event, never an incidental
/// driver-mount or user binding. Three guards keep it honoured:
///
/// 1. [`MountRegistry::register`] rejects a driver mount named after a realm (governance).
/// 2. [`peel_scope`] resolves a path's leading realm before routing the service path.
/// 3. [`resolve_name`] ranks a reserved realm name **above** every user-introduced name
///    (a `LET` binding, a connection) so a realm is never shadowed.
pub const RESERVED_REALMS: [&str; 6] = ["members", "projects", "hosts", "directories", "me", "sys"];

/// One of the closed set of [`RESERVED_REALMS`] (decision P / §1.3) — the *scope* axis of
/// a path. A path resolves to exactly one realm; a bare path (`/sql/pg/orders`) is sugar
/// for the self realm [`Realm::Me`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Realm {
    /// `/members/<principal>/…` — another human's world (needs a `POLICY` grant).
    Members,
    /// `/projects/<principal>/…` — a project/team world.
    Projects,
    /// `/hosts/<principal>/…` — an agent-fabric host world (M7).
    Hosts,
    /// `/directories/<principal>/…` — a directory/collection world.
    Directories,
    /// `/me/…` — the caller's own world; the realm a bare path desugars to.
    Me,
    /// `/sys/…` — the admin realm. The one **driver-backed** realm (its mount and realm
    /// coincide): the `/sys` driver serves it, so a driver *may* mount here.
    Sys,
}

impl Realm {
    /// The realm a leading path segment names, or `None` if the segment is an ordinary
    /// (non-realm) name. This is the single source of truth for "is this a realm".
    #[must_use]
    pub fn from_segment(segment: &str) -> Option<Self> {
        Some(match segment {
            "members" => Self::Members,
            "projects" => Self::Projects,
            "hosts" => Self::Hosts,
            "directories" => Self::Directories,
            "me" => Self::Me,
            "sys" => Self::Sys,
            _ => return None,
        })
    }

    /// The realm's canonical segment spelling (`members`, `me`, …).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Members => "members",
            Self::Projects => "projects",
            Self::Hosts => "hosts",
            Self::Directories => "directories",
            Self::Me => "me",
            Self::Sys => "sys",
        }
    }

    /// Whether this is a **plural collection** realm — it takes exactly one principal
    /// segment (`/members/alice/…`). The singletons `me`/`sys` take none. Single-principal
    /// arity is one of the two §1.3 rules that keep `(scope, service)` decidable.
    #[must_use]
    pub const fn takes_principal(self) -> bool {
        matches!(
            self,
            Self::Members | Self::Projects | Self::Hosts | Self::Directories
        )
    }

    /// Whether the realm's service root is itself **driver-backed** — the admin realm
    /// `/sys`, whose realm and mount coincide. It is the one realm a driver may mount
    /// under (the [`MountRegistry::register`] governance exempts it), and [`peel_scope`]
    /// keeps the whole `/sys/…` path as the service path so the sys driver still routes it.
    #[must_use]
    pub const fn is_driver_backed(self) -> bool {
        matches!(self, Self::Sys)
    }
}

/// A resolved **scope** (decision P / §1.3): the realm a path lives in plus, for a
/// collection realm, the single principal segment that selects *whose* world it is. The
/// singletons carry no principal (`Me`/`Sys`). Threaded downstream so later stages
/// (credential resolution, `POLICY`) know *whose* world a node belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathScope {
    /// The realm axis of the path.
    pub realm: Realm,
    /// The principal segment for a collection realm (`alice` in `/members/alice/…`), or
    /// `None` for the `me`/`sys` singletons.
    pub principal: Option<String>,
}

impl PathScope {
    /// The self scope — what a bare path (`/sql/pg/orders`) desugars to.
    #[must_use]
    pub fn me() -> Self {
        Self {
            realm: Realm::Me,
            principal: None,
        }
    }
}

/// The outcome of peeling a leading scope realm off a path ([`peel_scope`]): the resolved
/// [`PathScope`] and the remaining **service** path (with a leading `/`) to route against
/// driver mounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeResolution {
    /// The resolved scope (whose world).
    pub scope: PathScope,
    /// The remaining service path (what), routed against the [`MountRegistry`].
    pub service: String,
}

/// The structured, machine-readable failure of scope resolution (RFD §5). The two arms
/// are exactly the §1.3 boundary violations: a collection realm used without its single
/// principal, and a reserved realm name appearing in *service* position (a cross-realm
/// reference). Both are rejected rather than silently routed — relaxing either rule is
/// what reintroduces the ambiguity §1.3 calls out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathScopeError {
    /// A plural-collection realm (`/members`, …) appeared without its single principal
    /// segment, so *whose* world is undetermined.
    MissingPrincipal {
        /// The collection realm that lacked a principal.
        realm: &'static str,
    },
    /// A reserved realm name appeared in **service** position (e.g. `/me/members/…`) — a
    /// path names exactly one realm, so re-entering a realm from inside a service path is
    /// rejected rather than guessed.
    CrossRealm {
        /// The realm name found in service position.
        realm: &'static str,
        /// The full offending path.
        path: String,
    },
}

impl PathScopeError {
    /// A stable, machine-readable code an AI-facing caller branches on (RFD §5).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::MissingPrincipal { .. } => "missing_principal",
            Self::CrossRealm { .. } => "cross_realm",
        }
    }
}

/// How a bare leading **name** resolves under the decision-P precedence ladder (§1.3).
/// A reserved realm name is fixed and outranks every user-introduced name; below it sit
/// (in order) a lexical `LET` binding (t60), a driver mount, and a connection — then
/// nothing. The ranking is what guarantees a realm is **never shadowed** by a binding or
/// a connection, and that a `LET`-bound name (the lexical realm) wins over a mount realm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameRealm {
    /// A [`RESERVED_REALMS`] name — fixed realm, highest precedence, never shadowed.
    Reserved(Realm),
    /// A `LET` lexical binding in scope (t60) — the lexical realm.
    Lexical,
    /// A driver mount root — the mount realm.
    Mount,
    /// A connection (`<provider>/<account>`) key within the scope.
    Connection,
    /// None of the above — a typo'd / unbound bare name.
    Unbound,
}

/// Resolve a bare leading **name** under the decision-P precedence ladder (§1.3):
/// `Reserved` > `Lexical` (`LET`) > `Mount` > `Connection` > `Unbound`. A reserved realm
/// name short-circuits **above** `is_let_bound` and `is_connection`, so it can never be
/// shadowed by a user binding or a connection of the same spelling; a lexical binding
/// outranks a mount of the same spelling (t60's "consult the binding scope before the
/// mount registry").
#[must_use]
pub fn resolve_name(
    name: &str,
    is_let_bound: bool,
    is_mount: bool,
    is_connection: bool,
) -> NameRealm {
    if let Some(realm) = Realm::from_segment(name) {
        return NameRealm::Reserved(realm);
    }
    if is_let_bound {
        return NameRealm::Lexical;
    }
    if is_mount {
        return NameRealm::Mount;
    }
    if is_connection {
        return NameRealm::Connection;
    }
    NameRealm::Unbound
}

/// Peel a leading scope realm off `path`, splitting it into `(scope, service)` (decision
/// P / §1.3). Recognizes a leading [`Realm`]: a collection realm consumes **exactly one**
/// principal segment; the `me` singleton is peeled to its remainder; the driver-backed
/// `sys` singleton keeps the whole `/sys/…` path as the service path (its mount and realm
/// coincide); and a bare path (no realm prefix) is sugar for the self realm [`Realm::Me`]
/// with the whole path as the service path. The service path is then routed against the
/// driver mounts by [`MountRegistry::resolve_path`].
///
/// # Errors
/// [`PathScopeError::MissingPrincipal`] if a collection realm has no principal segment;
/// [`PathScopeError::CrossRealm`] if the service path re-enters a reserved realm.
pub fn peel_scope(path: &str) -> Result<ScopeResolution, PathScopeError> {
    let trimmed = path.trim_start_matches('/');
    let segments: Vec<&str> = if trimmed.is_empty() {
        Vec::new()
    } else {
        trimmed.split('/').collect()
    };

    let Some(&first) = segments.first() else {
        // The empty / root path is the self realm.
        return Ok(ScopeResolution {
            scope: PathScope::me(),
            service: "/".to_string(),
        });
    };

    let Some(realm) = Realm::from_segment(first) else {
        // A bare path is sugar for `/me/…`; the whole path is the service path.
        return Ok(ScopeResolution {
            scope: PathScope::me(),
            service: join_service(&segments),
        });
    };

    // The driver-backed admin realm: realm and mount coincide, so the whole `/sys/…`
    // path stays the service path (the sys driver routes it as before).
    if realm.is_driver_backed() {
        return Ok(ScopeResolution {
            scope: PathScope {
                realm,
                principal: None,
            },
            service: join_service(&segments),
        });
    }

    if realm.takes_principal() {
        let Some(&principal) = segments.get(1) else {
            return Err(PathScopeError::MissingPrincipal {
                realm: realm.as_str(),
            });
        };
        let service = &segments[2..];
        reject_cross_realm(service, path)?;
        return Ok(ScopeResolution {
            scope: PathScope {
                realm,
                principal: Some(principal.to_string()),
            },
            service: join_service(service),
        });
    }

    // The `me` singleton: peel `/me`; the remainder is the service path.
    let service = &segments[1..];
    reject_cross_realm(service, path)?;
    Ok(ScopeResolution {
        scope: PathScope::me(),
        service: join_service(service),
    })
}

/// Render a service segment slice back into a `/seg/seg` path (a `/` for the empty slice).
fn join_service(segments: &[&str]) -> String {
    if segments.is_empty() {
        return "/".to_string();
    }
    let mut s = String::new();
    for seg in segments {
        s.push('/');
        s.push_str(seg);
    }
    s
}

/// Reject a service path whose leading segment re-enters a reserved realm (§1.3: a path
/// names exactly one realm).
fn reject_cross_realm(service: &[&str], full: &str) -> Result<(), PathScopeError> {
    if let Some(&first) = service.first() {
        if let Some(inner) = Realm::from_segment(first) {
            return Err(PathScopeError::CrossRealm {
                realm: inner.as_str(),
                path: full.to_string(),
            });
        }
    }
    Ok(())
}

/// Registry of path mounts → drivers (RFD-0001 §3, "paths"). Keyed by mount string
/// (`/mail`, `/s3`, …).
///
/// `Clone` is cheap (the map holds `Arc<dyn Driver>`), enabling the t28 shell completer to
/// hand an owned snapshot to a timeout-bounded scan thread without holding a borrow across the
/// thread boundary.
#[derive(Default, Clone)]
pub struct MountRegistry {
    mounts: BTreeMap<String, Arc<dyn Driver>>,
}

impl MountRegistry {
    /// An empty mount registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a driver under its declared mount.
    ///
    /// Enforces the decision-P **governance rule** (§1.3): a driver mount must not be
    /// named after a reserved scope realm ([`RESERVED_REALMS`]), because the
    /// `(scope, service)` split a path resolves through is only decidable if no mount
    /// shadows a realm. The sole driver-backed realm `/sys` is exempt (its realm and
    /// mount coincide — see [`Realm::is_driver_backed`]).
    ///
    /// # Errors
    /// [`CfsError::ReservedRealmMount`] if the mount's leading segment shadows a (non
    /// driver-backed) realm; [`CfsError::DuplicateRegistration`] if the mount is already
    /// taken.
    pub fn register(&mut self, driver: Arc<dyn Driver>) -> Result<(), CfsError> {
        let key = driver.mount().to_string();
        let leading = key.trim_start_matches('/').split('/').next().unwrap_or("");
        if let Some(realm) = Realm::from_segment(leading) {
            if !realm.is_driver_backed() {
                return Err(CfsError::ReservedRealmMount {
                    mount: key,
                    realm: realm.as_str(),
                });
            }
        }
        if self.mounts.contains_key(&key) {
            return Err(CfsError::DuplicateRegistration(key));
        }
        self.mounts.insert(key, driver);
        Ok(())
    }

    /// Register `driver` under an EXPLICIT `alias` mount, in ADDITION to its declared `mount()`.
    /// Used for a **deprecated path alias** kept working for one release (e.g. `/ga` →
    /// `/google-analytics`): the same driver answers both prefixes, so an old path still routes while
    /// the canonical mount (what `mount()` returns, and what the docs render) is the new name. The
    /// alias is a runtime-routing entry only — it is NOT a second `mount()`, so introspection/docs
    /// keep showing the canonical name. (The general user-facing `CREATE ALIAS` mechanism is separate
    /// future work; this is the built-in deprecation shim.)
    ///
    /// # Errors
    /// [`CfsError::ReservedRealmMount`] if `alias`'s leading segment shadows a non-driver-backed
    /// realm; [`CfsError::DuplicateRegistration`] if `alias` is already taken.
    pub fn register_alias(&mut self, alias: &str, driver: Arc<dyn Driver>) -> Result<(), CfsError> {
        let key = alias.to_string();
        let leading = key.trim_start_matches('/').split('/').next().unwrap_or("");
        if let Some(realm) = Realm::from_segment(leading) {
            if !realm.is_driver_backed() {
                return Err(CfsError::ReservedRealmMount {
                    mount: key,
                    realm: realm.as_str(),
                });
            }
        }
        if self.mounts.contains_key(&key) {
            return Err(CfsError::DuplicateRegistration(key));
        }
        self.mounts.insert(key, driver);
        Ok(())
    }

    /// Resolve a mount to its driver.
    ///
    /// # Errors
    /// [`CfsError::UnknownMount`] if no driver is registered for the mount.
    pub fn resolve(&self, mount: &str) -> Result<Arc<dyn Driver>, CfsError> {
        self.mounts
            .get(mount)
            .cloned()
            .ok_or_else(|| CfsError::UnknownMount(mount.to_string()))
    }

    /// Route a full path to the driver whose mount is the **longest prefix** of it,
    /// returning that driver and the remaining **sub-path** (the path with the matched
    /// mount and its trailing `/` stripped). Overlapping mounts (`/g` and `/git`)
    /// resolve to the longest match, so `/git/repo@ref/x` routes to the `/git` driver
    /// with sub-path `repo@ref/x` (never to `/g`).
    ///
    /// A mount matches only at a path **boundary**: it must equal the path, or the path
    /// must continue with `/` after it — so `/git` does not capture `/gitlab/x`. Returns
    /// `None` when no mount is a boundary-prefix of `path` (the caller raises
    /// [`CfsError::UnknownMount`] with context it owns).
    #[must_use]
    pub fn resolve_path(&self, path: &str) -> Option<(Arc<dyn Driver>, String)> {
        let mut best: Option<(&String, &Arc<dyn Driver>)> = None;
        for (mount, driver) in &self.mounts {
            let matches = path == mount
                || path
                    .strip_prefix(mount.as_str())
                    .is_some_and(|rest| rest.starts_with('/'));
            if matches && best.is_none_or(|(b, _)| mount.len() > b.len()) {
                best = Some((mount, driver));
            }
        }
        best.map(|(mount, driver)| {
            let sub = path
                .strip_prefix(mount.as_str())
                .unwrap_or("")
                .trim_start_matches('/')
                .to_string();
            (Arc::clone(driver), sub)
        })
    }

    /// Iterate every registered driver (deterministic mount order). Used by name
    /// resolution (t06) to find which drivers ship a given prelude alias when deciding
    /// receiver-typed alias scope / ambiguity.
    pub fn drivers(&self) -> impl Iterator<Item = &Arc<dyn Driver>> {
        self.mounts.values()
    }

    /// Number of registered mounts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.mounts.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }
}

/// Registry of functions + `CALL` procedures (RFD-0001 §3, "functions /
/// procedures"). One registry because both alias functions and procedures are
/// receiver-typed, registry-resolved, and keyword-free. Keyed by qualified name
/// (e.g. `mail.send`). Stores the [`ProcSig`] declaration (params, irreversible,
/// returns, requires_scopes — t13) only.
#[derive(Default)]
pub struct ProcRegistry {
    procs: BTreeMap<String, ProcSig>,
}

impl ProcRegistry {
    /// An empty procedure registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a procedure under a qualified name (e.g. `mail.send`).
    ///
    /// # Errors
    /// [`CfsError::DuplicateRegistration`] if the name is already taken.
    pub fn register(&mut self, qualified_name: &str, decl: ProcSig) -> Result<(), CfsError> {
        if self.procs.contains_key(qualified_name) {
            return Err(CfsError::DuplicateRegistration(qualified_name.to_string()));
        }
        self.procs.insert(qualified_name.to_string(), decl);
        Ok(())
    }

    /// Resolve a qualified procedure name to its declaration.
    ///
    /// # Errors
    /// [`CfsError::UnknownProcedure`] if the name is not registered.
    pub fn resolve(&self, qualified_name: &str) -> Result<&ProcSig, CfsError> {
        self.procs
            .get(qualified_name)
            .ok_or_else(|| CfsError::UnknownProcedure(qualified_name.to_string()))
    }

    /// Number of registered procedures.
    #[must_use]
    pub fn len(&self) -> usize {
        self.procs.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.procs.is_empty()
    }
}

/// Registry of codecs (RFD-0001 §3, "codecs"). Keyed by format (`json`, `yaml`, …).
#[derive(Default)]
pub struct CodecRegistry {
    codecs: BTreeMap<String, Arc<dyn Codec>>,
}

impl CodecRegistry {
    /// An empty codec registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry pre-loaded with the six builtin codecs (`json`, `jsonl`, `yaml`,
    /// `toml`, `csv`, `md+frontmatter`) from `qfs-codec` (t15). This is the default the
    /// engine resolves `DECODE`/`ENCODE fmt` through; a backend extends it via
    /// [`CodecRegistry::register`] (a new codec = zero keywords, RFD §3).
    ///
    /// The builtins have distinct format names, so registration never collides; the
    /// `expect_used` lint is satisfied without an `unwrap` because the only error arm
    /// ([`CfsError::DuplicateRegistration`]) is structurally unreachable here.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        for codec in qfs_codec::builtin_codecs() {
            // Builtin format names are unique by construction; ignore the (unreachable)
            // duplicate error rather than panic, keeping lib code panic-free.
            let _ = reg.register(codec);
        }
        reg
    }

    /// Register a codec under its declared format.
    ///
    /// # Errors
    /// [`CfsError::DuplicateRegistration`] if the format is already taken.
    pub fn register(&mut self, codec: Arc<dyn Codec>) -> Result<(), CfsError> {
        let key = codec.fmt().to_string();
        if self.codecs.contains_key(&key) {
            return Err(CfsError::DuplicateRegistration(key));
        }
        self.codecs.insert(key, codec);
        Ok(())
    }

    /// Resolve a format to its codec.
    ///
    /// # Errors
    /// [`CfsError::UnknownCodec`] if no codec is registered for the format.
    pub fn resolve(&self, fmt: &str) -> Result<Arc<dyn Codec>, CfsError> {
        self.codecs
            .get(fmt)
            .cloned()
            .ok_or_else(|| CfsError::UnknownCodec(fmt.to_string()))
    }

    /// Number of registered codecs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.codecs.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.codecs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_codec::{Codec, RowBatch};
    use qfs_driver::{Archetype, Capabilities, NodeDesc, Path, PushdownProfile, VersionSupport};
    use qfs_plan::{AppliedEffect, ApplyError, EffectNode, PlanApplier};
    use qfs_types::Schema;

    /// A no-I/O applier the fake driver hands back through the `applier()` seam.
    #[derive(Default)]
    struct NoopApplier;
    impl PlanApplier for NoopApplier {
        fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
            Ok(AppliedEffect::new(node.id, 0))
        }
    }

    struct FakeDriver {
        mount: &'static str,
        pushdown: PushdownProfile,
        applier: NoopApplier,
    }
    impl FakeDriver {
        fn new() -> Self {
            Self::at("/fake")
        }
        fn at(mount: &'static str) -> Self {
            Self {
                mount,
                pushdown: PushdownProfile::None,
                applier: NoopApplier,
            }
        }
    }
    impl Driver for FakeDriver {
        fn mount(&self) -> &str {
            self.mount
        }
        fn describe(&self, _p: &Path) -> Result<NodeDesc, CfsError> {
            let _ = NodeDesc::new(Archetype::BlobNamespace, Schema::empty());
            Err(CfsError::NotImplemented {
                feature: "describe",
            })
        }
        fn capabilities(&self, _p: &Path) -> Capabilities {
            Capabilities::default()
        }
        fn procedures(&self) -> &[ProcSig] {
            &[]
        }
        fn pushdown(&self) -> &PushdownProfile {
            &self.pushdown
        }
        fn version_support(&self, _p: &Path) -> VersionSupport {
            VersionSupport::None
        }
        fn applier(&self) -> &dyn PlanApplier {
            &self.applier
        }
    }

    struct FakeCodec;
    impl Codec for FakeCodec {
        fn fmt(&self) -> &str {
            "fake"
        }
        fn decode(&self, _b: &[u8]) -> Result<RowBatch, CfsError> {
            Err(CfsError::NotImplemented { feature: "decode" })
        }
        fn encode(&self, _r: &RowBatch) -> Result<Vec<u8>, CfsError> {
            Err(CfsError::NotImplemented { feature: "encode" })
        }
    }

    #[test]
    fn mount_registry_empty_then_roundtrip_then_duplicate_then_absent() {
        let mut reg = MountRegistry::new();
        assert!(reg.is_empty());
        assert!(matches!(
            reg.resolve("/fake"),
            Err(CfsError::UnknownMount(_))
        ));

        reg.register(Arc::new(FakeDriver::new())).unwrap();
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.resolve("/fake").unwrap().mount(), "/fake");

        let dup = reg.register(Arc::new(FakeDriver::new()));
        assert!(matches!(dup, Err(CfsError::DuplicateRegistration(_))));
    }

    /// O1 — the longest-mount-prefix router: overlapping mounts (`/g`, `/git`) resolve to
    /// the longest match, the matched mount is stripped to a sub-path, and an unmatched
    /// path returns `None`. Also proves the boundary rule (`/git` ≠ `/gitlab/...`).
    #[test]
    fn resolve_path_picks_longest_mount_prefix() {
        let mut reg = MountRegistry::new();
        reg.register(Arc::new(FakeDriver::at("/g"))).unwrap();
        reg.register(Arc::new(FakeDriver::at("/git"))).unwrap();

        // Longest match wins: /git, not /g.
        let (driver, sub) = reg.resolve_path("/git/repo@ref/x").unwrap();
        assert_eq!(driver.mount(), "/git");
        assert_eq!(sub, "repo@ref/x");

        // The shorter mount still routes its own subtree.
        let (driver, sub) = reg.resolve_path("/g/foo").unwrap();
        assert_eq!(driver.mount(), "/g");
        assert_eq!(sub, "foo");

        // Exact-mount path yields an empty sub-path.
        let (driver, sub) = reg.resolve_path("/git").unwrap();
        assert_eq!(driver.mount(), "/git");
        assert_eq!(sub, "");

        // Boundary rule: /git must not capture /gitlab/* — and with no /gitlab mount,
        // there is no boundary-prefix at all, so it is unmatched.
        assert!(reg.resolve_path("/gitlab/x").is_none());

        // Wholly unmatched path → None.
        assert!(reg.resolve_path("/s3/bucket/key").is_none());
    }

    #[test]
    fn proc_registry_empty_then_roundtrip_then_duplicate_then_absent() {
        let mut reg = ProcRegistry::new();
        assert!(reg.is_empty());
        assert!(matches!(
            reg.resolve("mail.send"),
            Err(CfsError::UnknownProcedure(_))
        ));

        let decl = ProcSig::new("send");
        reg.register("mail.send", decl.clone()).unwrap();
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.resolve("mail.send").unwrap().name, "send");

        let dup = reg.register("mail.send", decl);
        assert!(matches!(dup, Err(CfsError::DuplicateRegistration(_))));
    }

    #[test]
    fn codec_registry_empty_then_roundtrip_then_duplicate_then_absent() {
        let mut reg = CodecRegistry::new();
        assert!(reg.is_empty());
        assert!(matches!(
            reg.resolve("fake"),
            Err(CfsError::UnknownCodec(_))
        ));

        reg.register(Arc::new(FakeCodec)).unwrap();
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.resolve("fake").unwrap().fmt(), "fake");

        let dup = reg.register(Arc::new(FakeCodec));
        assert!(matches!(dup, Err(CfsError::DuplicateRegistration(_))));
    }

    // ---- Decision P (t71): scope realms, governance, reserved-name resolution --------

    /// Governance (§1.3): a driver mount named after a reserved realm is rejected at
    /// registration — that is what keeps the `(scope, service)` split decidable. The
    /// driver-backed admin realm `/sys` is the sole exception (realm and mount coincide).
    #[test]
    fn register_rejects_a_driver_mount_that_shadows_a_realm() {
        let mut reg = MountRegistry::new();
        for realm in ["/members", "/projects", "/hosts", "/directories", "/me"] {
            let err = reg.register(Arc::new(FakeDriver::at(realm))).unwrap_err();
            assert_eq!(err.code(), "reserved_realm_mount");
            assert!(matches!(err, CfsError::ReservedRealmMount { mount, .. } if mount == realm));
        }
        // The driver-backed `/sys` realm is exempt — its realm and mount coincide.
        reg.register(Arc::new(FakeDriver::at("/sys"))).unwrap();
        assert_eq!(reg.resolve("/sys").unwrap().mount(), "/sys");
        // A non-realm mount is unaffected.
        reg.register(Arc::new(FakeDriver::at("/mail"))).unwrap();
    }

    /// A path resolves to the right realm by precedence: a collection realm consumes one
    /// principal and strips it from the service path; `/me/…` peels to its remainder; the
    /// driver-backed `/sys/…` keeps the whole path; a bare path is sugar for `/me`.
    #[test]
    fn peel_scope_resolves_each_realm_by_precedence() {
        // Collection realm + single principal → scope carries the principal; service is
        // the stripped remainder.
        let r = peel_scope("/members/alice/gmail/inbox").unwrap();
        assert_eq!(r.scope.realm, Realm::Members);
        assert_eq!(r.scope.principal.as_deref(), Some("alice"));
        assert_eq!(r.service, "/gmail/inbox");

        // `/me` singleton peels to its remainder.
        let r = peel_scope("/me/google/work/gmail/inbox").unwrap();
        assert_eq!(r.scope, PathScope::me());
        assert_eq!(r.service, "/google/work/gmail/inbox");

        // The driver-backed `/sys` realm keeps the whole path as the service path so the
        // sys driver still routes it.
        let r = peel_scope("/sys/audit").unwrap();
        assert_eq!(r.scope.realm, Realm::Sys);
        assert_eq!(r.scope.principal, None);
        assert_eq!(r.service, "/sys/audit");

        // A bare path is sugar for the self realm; the whole path is the service path.
        let r = peel_scope("/sql/pg/orders").unwrap();
        assert_eq!(r.scope, PathScope::me());
        assert_eq!(r.service, "/sql/pg/orders");

        // A one-level `*` is a legal principal (glob over the collection, §1.3 step 4).
        let r = peel_scope("/members/*/gmail/inbox").unwrap();
        assert_eq!(r.scope.principal.as_deref(), Some("*"));
        assert_eq!(r.service, "/gmail/inbox");
    }

    /// A cross-realm / ambiguous reference is a **structured** error, never a silent
    /// route: a collection realm without a principal, and a service path that re-enters a
    /// realm, are exactly the two §1.3 boundary violations.
    #[test]
    fn peel_scope_rejects_cross_realm_and_missing_principal() {
        let err = peel_scope("/members").unwrap_err();
        assert_eq!(err.code(), "missing_principal");
        assert!(matches!(err, PathScopeError::MissingPrincipal { realm } if realm == "members"));

        // A service path may not re-enter a realm (a path names exactly one realm).
        let err = peel_scope("/me/members/alice").unwrap_err();
        assert_eq!(err.code(), "cross_realm");
        assert!(matches!(err, PathScopeError::CrossRealm { realm, .. } if realm == "members"));
    }

    /// Reserved-name resolution (§1.3): a reserved realm name resolves to its fixed realm
    /// and is **not** shadowed by a `LET` binding or a connection of the same spelling;
    /// and the lexical (`LET`) realm outranks the mount realm (t60 precedence).
    #[test]
    fn resolve_name_ranks_realm_above_binding_and_lexical_above_mount() {
        // A reserved realm wins even when a binding AND a connection share its spelling.
        assert_eq!(
            resolve_name("sys", /*let*/ true, /*mount*/ true, /*conn*/ true),
            NameRealm::Reserved(Realm::Sys),
        );
        assert_eq!(
            resolve_name("members", true, false, true),
            NameRealm::Reserved(Realm::Members),
        );
        // A lexical `LET` binding outranks a mount of the same spelling (t60).
        assert_eq!(
            resolve_name("orders", /*let*/ true, /*mount*/ true, /*conn*/ false),
            NameRealm::Lexical,
        );
        // With no binding, the mount realm wins over a connection.
        assert_eq!(resolve_name("orders", false, true, true), NameRealm::Mount,);
        // A connection name (no realm/binding/mount) resolves to the connection realm.
        assert_eq!(
            resolve_name("work", false, false, true),
            NameRealm::Connection
        );
        // A typo is unbound.
        assert_eq!(
            resolve_name("ghost", false, false, false),
            NameRealm::Unbound
        );
    }

    /// t15 — `with_builtins` resolves all six builtin codecs by name, and an unknown
    /// format returns a structured `UnknownCodec` (not a panic).
    #[test]
    fn codec_registry_with_builtins_resolves_all_six() {
        let reg = CodecRegistry::with_builtins();
        assert_eq!(reg.len(), 6);
        for fmt in ["json", "jsonl", "yaml", "toml", "csv", "md"] {
            assert_eq!(reg.resolve(fmt).unwrap().fmt(), fmt);
        }
        assert!(matches!(
            reg.resolve("parquet"),
            Err(CfsError::UnknownCodec(_))
        ));
    }
}
