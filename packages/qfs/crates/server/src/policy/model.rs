//! Owned, vendor-free policy DTOs (RFD-0001 §8/§9/§10): [`Verb`], [`Effectivity`],
//! [`VerbSet`], [`DriverGlob`], [`Rule`], and [`Policy`].
//!
//! A [`Policy`] is the **may** layer (RFD §10 least privilege): per handler, the set of
//! `(verb, driver)` pairs that handler's COMMIT plan may touch. It is **pure data** — no
//! I/O, no vendor handle, no credential — so it round-trips through `/server/policies` rows
//! and the pure enforcer ([`super::enforce::evaluate`]) classifies a plan against it with no
//! live creds.
//!
//! ## Default-deny (the single most important behavior)
//! [`Policy::default`] is `default: Effectivity::Deny` with **no rules** — a handler with no
//! policy, or an empty policy, **denies every effect** (fail closed). A policy only *widens*
//! the closed default via explicit `ALLOW` rules.

use std::collections::{BTreeMap, BTreeSet};

use qfs_core::Realm;
use serde::{Deserialize, Serialize};

/// A universal write/read verb (RFD §3/§5). The closed-core verb taxonomy the policy
/// vocabulary speaks: a plan effect node is classified into exactly one of these, and a
/// [`Rule`] grants/denies a [`VerbSet`] of them. Mirrors `qfs_driver::Verb` /
/// `qfs_core::EffectKind` but is **owned by the policy layer** (no vendor leak): the verb a
/// policy reasons about is the *intent* (SELECT/INSERT/…), not a driver-internal op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verb {
    /// `SELECT` — a read (routed through the read path, not the commit plan; see module docs).
    Select,
    /// `INSERT INTO`.
    Insert,
    /// `UPSERT INTO` — idempotent create-or-update.
    Upsert,
    /// `UPDATE`.
    Update,
    /// `REMOVE` — destructive / irreversible (RFD §10).
    Remove,
    /// `CALL` — an irreducible namespaced procedure; may be irreversible (e.g. `mail.send`).
    Call,
}

impl Verb {
    /// Every verb, in canonical order (the order the freeze test + `VerbSet::all` use).
    pub const ALL: [Verb; 6] = [
        Verb::Select,
        Verb::Insert,
        Verb::Upsert,
        Verb::Update,
        Verb::Remove,
        Verb::Call,
    ];

    /// A short, stable, secret-free label for golden snapshots, grammar, and audit records.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Verb::Select => "SELECT",
            Verb::Insert => "INSERT",
            Verb::Upsert => "UPSERT",
            Verb::Update => "UPDATE",
            Verb::Remove => "REMOVE",
            Verb::Call => "CALL",
        }
    }

    /// Parse a verb from its canonical uppercase label. Used by the grammar (parsing a verb
    /// list) and the round-trip rehydrate. `None` for any other word.
    #[must_use]
    pub fn from_label(word: &str) -> Option<Self> {
        Some(match word {
            "SELECT" => Verb::Select,
            "INSERT" => Verb::Insert,
            "UPSERT" => Verb::Upsert,
            "UPDATE" => Verb::Update,
            "REMOVE" => Verb::Remove,
            "CALL" => Verb::Call,
            _ => return None,
        })
    }

    /// The single-bit mask this verb occupies in a [`VerbSet`].
    const fn bit(self) -> u8 {
        match self {
            Verb::Select => 1 << 0,
            Verb::Insert => 1 << 1,
            Verb::Upsert => 1 << 2,
            Verb::Update => 1 << 3,
            Verb::Remove => 1 << 4,
            Verb::Call => 1 << 5,
        }
    }

    /// Whether this verb is **irreversible** for the strictness rule (RFD §6/§10): `REMOVE`
    /// is inherently destructive and `CALL` may be a declared-irreversible procedure. The
    /// enforcer requires these to be granted by an *explicit* verb in the rule's [`VerbSet`],
    /// never folded in by a bare `ALLOW ALL` (see [`super::enforce`]).
    #[must_use]
    pub const fn is_irreversible_class(self) -> bool {
        matches!(self, Verb::Remove | Verb::Call)
    }
}

/// Whether a [`Rule`] grants (`Allow`) or refuses (`Deny`) its verbs on its driver scope. Also
/// the `Policy::default` effectivity — `Deny` is the fail-closed default (RFD §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effectivity {
    /// Grant the verbs (widen from the closed default).
    Allow,
    /// Refuse the verbs (the default-deny baseline, or an explicit deny that overrides an
    /// earlier allow when it appears later in rule order).
    Deny,
}

impl Effectivity {
    /// The grammar/round-trip keyword (`ALLOW`/`DENY`).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Effectivity::Allow => "ALLOW",
            Effectivity::Deny => "DENY",
        }
    }

    /// Parse from the `ALLOW`/`DENY` keyword. `None` otherwise.
    #[must_use]
    pub fn from_label(word: &str) -> Option<Self> {
        match word {
            "ALLOW" => Some(Effectivity::Allow),
            "DENY" => Some(Effectivity::Deny),
            _ => None,
        }
    }
}

/// A bitflags-style set of [`Verb`]s. Implemented over a `u8` mask (no `bitflags` crate
/// dependency — the closed verb taxonomy fits in 6 bits). `ALL` is every verb.
///
/// **Strictness note (RFD §6/§10):** `ALL` *does* include `REMOVE` and `CALL`, but the
/// enforcer never lets a `Rule` whose verbs came from a bare `ALL` grant those irreversible
/// classes — see [`Rule::is_broad_all`] and [`super::enforce`]. `ALL` as a *literal* set is
/// distinct from "the rule was written as `ALLOW ALL`": the [`Rule`] carries the latter bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VerbSet(u8);

impl VerbSet {
    /// The empty set (no verbs).
    pub const EMPTY: VerbSet = VerbSet(0);

    /// Every verb (the `ALL` token's expansion).
    #[must_use]
    pub fn all() -> Self {
        let mut bits = 0u8;
        for v in Verb::ALL {
            bits |= v.bit();
        }
        VerbSet(bits)
    }

    /// A single-verb set.
    #[must_use]
    pub fn one(verb: Verb) -> Self {
        VerbSet(verb.bit())
    }

    /// Build a set from a verb slice (the comma-list grammar form).
    #[must_use]
    pub fn from_verbs(verbs: &[Verb]) -> Self {
        let mut bits = 0u8;
        for &v in verbs {
            bits |= v.bit();
        }
        VerbSet(bits)
    }

    /// Add a verb (builder).
    #[must_use]
    pub fn with(mut self, verb: Verb) -> Self {
        self.0 |= verb.bit();
        self
    }

    /// Whether `verb` is in the set.
    #[must_use]
    pub fn contains(self, verb: Verb) -> bool {
        self.0 & verb.bit() != 0
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether the set is exactly every verb (the `ALL` literal).
    #[must_use]
    pub fn is_all(self) -> bool {
        self == VerbSet::all()
    }

    /// The verbs in the set, in canonical order (for round-trip rendering + golden output).
    #[must_use]
    pub fn verbs(self) -> Vec<Verb> {
        Verb::ALL
            .into_iter()
            .filter(|&v| self.contains(v))
            .collect()
    }
}

/// A driver-scope glob (RFD §8): matches the **leading `/driver/...` segment(s)** of an
/// effect's target path. `mail` matches `/mail/...`; `s3/*` matches `/s3/<anything>/...`. An
/// owned, opaque string — no vendor handle. An empty glob (the `ON`-less rule) matches every
/// driver (scope-wide).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverGlob(pub String);

impl DriverGlob {
    /// Construct a glob from owned text.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        DriverGlob(raw.into())
    }

    /// The match-every-driver glob (an `ON`-less rule).
    #[must_use]
    pub fn any() -> Self {
        DriverGlob(String::new())
    }

    /// Whether this glob matches every driver (empty glob).
    #[must_use]
    pub fn is_any(&self) -> bool {
        self.0.is_empty()
    }

    /// The raw glob text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this glob matches the driver implied by a target `path` (`/mail/inbox` →
    /// driver segment `mail`) AND the `driver` id. Matching is over the **leading path
    /// segments**, not driver internals (RFD §8): the policy reads the plan node's
    /// already-carried `(driver, path)`, never re-derives from the driver.
    ///
    /// An empty glob matches everything. A glob `s3/*` matches `/s3/<anything>/...`. A glob
    /// `mail` matches a path whose first segment is `mail` (or the driver id `mail`).
    #[must_use]
    pub fn matches(&self, driver: &str, path: &str) -> bool {
        if self.is_any() {
            return true;
        }
        let path_segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let glob_segments: Vec<&str> = self.0.trim_start_matches('/').split('/').collect();

        // The candidate leading segments: prefer the path segments, but also accept the
        // driver id as the leading segment when the path has none (a bare driver target).
        let head: Vec<&str> = if path_segments.first().is_some_and(|s| !s.is_empty()) {
            path_segments
        } else {
            vec![driver]
        };

        // Match glob segments against the leading path segments. A `*` matches any one
        // segment; a literal must equal. The glob must not be longer than the path head.
        if glob_segments.len() > head.len() {
            return false;
        }
        for (g, p) in glob_segments.iter().zip(head.iter()) {
            if *g == "*" {
                continue;
            }
            if g != p {
                return false;
            }
        }
        true
    }
}

/// **Who** a [`Rule`] is *for* (t57 — the actor/subject dimension, decision I / roadmap §1.2).
/// The richer ACL grows a *who* axis over today's *what* (verb) and *where* (driver/path): a
/// rule may be scoped to a single user (`qfs-identity` `UserId`-shaped, carried as an OWNED
/// string id — t45), a coarse role label (t55 `Role`, again as an owned string, NOT a pulled
/// identity type — the dep-direction guard keeps `qfs-server` off `qfs-identity`), or a group.
///
/// ## Default-deny preserved
/// [`Subject::Anyone`] is the *unscoped* subject — a rule with no `FOR` clause applies to every
/// actor (exactly the pre-t57 behaviour, so an old policy is unchanged). A `User`/`Role`/`Group`
/// subject **narrows** the rule to a matching actor: under the anonymous decision context (no
/// actor resolved) such a rule never matches, so the plan falls to default-deny (fail closed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subject {
    /// The unscoped subject — applies to every actor (the pre-t57 `FOR`-less rule).
    Anyone,
    /// A single user, by owned id string (`UserId` rendered — the binary maps the live
    /// `qfs-identity` `UserId` onto this; the policy layer never pulls the identity crate).
    User(String),
    /// A coarse role label (t55 `Role`: `owner`/`admin`/`member`, or a project role). Resolved
    /// against the actor's (inheritance-expanded) role set in the decision context.
    Role(String),
    /// A named group/team. Resolved against the actor's group set in the decision context.
    Group(String),
}

impl Default for Subject {
    /// The unscoped subject (every actor) — keeps a `FOR`-less rule behaving as before t57.
    fn default() -> Self {
        Subject::Anyone
    }
}

impl Subject {
    /// Whether this subject is the unscoped `Anyone` (no `FOR` clause).
    #[must_use]
    pub fn is_anyone(&self) -> bool {
        matches!(self, Subject::Anyone)
    }

    /// The canonical, secret-free round-trip form: `user:<id>` / `role:<name>` / `group:<name>`.
    /// [`Subject::Anyone`] renders as `anyone` (callers omit the whole `FOR` clause for it).
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Subject::Anyone => "anyone".to_string(),
            Subject::User(u) => format!("user:{u}"),
            Subject::Role(r) => format!("role:{r}"),
            Subject::Group(g) => format!("group:{g}"),
        }
    }

    /// Parse the canonical `user:`/`role:`/`group:` form (or `anyone`). `None` for anything
    /// else (a malformed stored subject — the caller drops the rule fail-closed).
    #[must_use]
    pub fn from_label(word: &str) -> Option<Self> {
        if word == "anyone" {
            return Some(Subject::Anyone);
        }
        let (kind, name) = word.split_once(':')?;
        if name.is_empty() {
            return None;
        }
        Some(match kind {
            "user" => Subject::User(name.to_string()),
            "role" => Subject::Role(name.to_string()),
            "group" => Subject::Group(name.to_string()),
            _ => return None,
        })
    }
}

/// A **realm-scoped path glob** (t57 row/path-level scope, built on t71 decision P realms). The
/// finest ACL axis: a rule may be narrowed to a sub-tree of *one* [`Realm`] — e.g. only
/// `/members/alice/...`. Matching is realm-gated (a glob anchored in [`Realm::Members`] never
/// matches a `/projects/...` node) **and** segment-wise within the realm.
///
/// Segment grammar: a literal segment must equal; `*` matches exactly one segment; a trailing
/// `**` matches the remaining segments (a sub-tree). The glob is stored as the full
/// realm-qualified path (`/members/alice/**`) so it round-trips verbatim through `/sys/policies`.
///
/// Stored as the canonical full realm-qualified path string ([`ScopeGlob::pattern`]) so it
/// round-trips verbatim through `/sys/policies` as plain data — the [`Realm`] axis is *derived*
/// from the leading segment on demand ([`ScopeGlob::realm`]), keeping the DTO serde-trivial and
/// `qfs-core`'s `Realm` free of a serde contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeGlob {
    /// The canonical full realm-qualified path glob (`/members/alice/**`). Always leading-slash
    /// normalized; never empty (constructed only via [`ScopeGlob::parse`]).
    pattern: String,
}

impl ScopeGlob {
    /// Parse a full realm-qualified path glob (`/members/alice/**`, `/me/mail/*`). A bare path
    /// (no leading realm segment) anchors in [`Realm::Me`] (the self realm — t71). `None` if the
    /// path is empty/`/` (a scope must name at least a realm/segment).
    #[must_use]
    pub fn parse(path: &str) -> Option<Self> {
        let segs: Vec<&str> = path
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        if segs.is_empty() {
            return None;
        }
        Some(ScopeGlob {
            pattern: format!("/{}", segs.join("/")),
        })
    }

    /// The canonical full realm-qualified path glob (`/members/alice/**`).
    #[must_use]
    pub fn render(&self) -> String {
        self.pattern.clone()
    }

    /// The [`Realm`] this scope is anchored in (the leading segment). A bare path is the self
    /// realm ([`Realm::Me`]). This is the realm gate axis: a node in a different realm never
    /// matches (decision P / §1.3).
    #[must_use]
    pub fn realm(&self) -> Realm {
        Self::peel(&self.pattern).0
    }

    /// Split a path into its `(realm, realm-relative segments)`: the leading realm segment names
    /// the realm; a bare path is the self realm and keeps all its segments. This is the single
    /// peel both the glob and the node path go through, so the principal segment (`alice` in
    /// `/members/alice/...`) is realm-relative on **both** sides and matches positionally.
    #[must_use]
    fn peel(path: &str) -> (Realm, Vec<&str>) {
        let mut segs = path
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty());
        let Some(first) = segs.next() else {
            return (Realm::Me, Vec::new());
        };
        match Realm::from_segment(first) {
            Some(r) => (r, segs.collect()),
            None => {
                let mut v = vec![first];
                v.extend(segs);
                (Realm::Me, v)
            }
        }
    }

    /// Whether this scope matches a node's full target `path`. The node's realm (peeled from its
    /// leading segment) must equal this scope's realm — the **realm gate**, so a `/members/...`
    /// scope never matches a `/projects/...` node — AND the realm-relative segments must
    /// glob-match.
    #[must_use]
    pub fn matches_path(&self, path: &str) -> bool {
        let (glob_realm, glob_segs) = Self::peel(&self.pattern);
        let (node_realm, node_segs) = Self::peel(path);
        if glob_realm != node_realm {
            return false;
        }
        glob_segments_match(&glob_segs, &node_segs)
    }
}

/// Segment-wise glob match: `*` matches one segment, a trailing `**` matches the remaining
/// segments (a sub-tree), a literal must equal. An empty glob matches every node (the whole
/// realm). The glob must consume the whole node path unless it ends in `**`.
fn glob_segments_match(glob: &[&str], node: &[&str]) -> bool {
    if glob.is_empty() {
        return true;
    }
    let mut gi = 0;
    let mut ni = 0;
    while gi < glob.len() {
        let g = glob[gi];
        if g == "**" {
            // Trailing `**` swallows the rest (zero or more segments).
            return true;
        }
        let Some(&n) = node.get(ni) else {
            return false;
        };
        if g != "*" && g != n {
            return false;
        }
        gi += 1;
        ni += 1;
    }
    // The glob is exhausted: it matches only if the node is also exhausted (no trailing `**`).
    ni == node.len()
}

/// A **conditional grant** (t57 `WHERE` clause). A rule may carry a pure predicate that must
/// hold for the grant to apply. The only predicate in t57 is the [`member_of`](Condition::MemberOf)
/// hook — a *function-valued* predicate (parsed as an ordinary `member_of('/directories/...')`
/// call, NOT a new keyword) whose truth is **resolved into the decision context** (never fetched
/// inside the pure enforcer). t58 supplies the live `/directories/...` resolver; t57 ships the
/// hook + a mockable resolver seam.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    /// No condition — the grant always applies (the `WHERE`-less rule).
    Always,
    /// `member_of('/directories/...')` — the grant applies only when the acting actor is a
    /// member of the named directory group. The directory path is an owned, secret-free ref.
    MemberOf(String),
}

impl Default for Condition {
    /// The unconditional grant — keeps a `WHERE`-less rule behaving as before t57.
    fn default() -> Self {
        Condition::Always
    }
}

impl Condition {
    /// Whether this is the unconditional [`Condition::Always`].
    #[must_use]
    pub fn is_always(&self) -> bool {
        matches!(self, Condition::Always)
    }

    /// The directory ref this condition tests, if it is a `member_of(...)`. Used by the gate to
    /// pre-resolve membership into the decision context (so the enforcer stays I/O-free).
    #[must_use]
    pub fn member_of_ref(&self) -> Option<&str> {
        match self {
            Condition::Always => None,
            Condition::MemberOf(dir) => Some(dir.as_str()),
        }
    }

    /// The canonical round-trip form for the `WHERE` clause, or `None` for [`Condition::Always`]
    /// (callers omit the whole `WHERE`). Renders as `member_of('/directories/...')` — the same
    /// surface the grammar parses.
    #[must_use]
    pub fn label(&self) -> Option<String> {
        match self {
            Condition::Always => None,
            Condition::MemberOf(dir) => Some(format!("member_of('{dir}')")),
        }
    }
}

/// A **role-inheritance graph** (t57 roles/groups/inheritance — decision flagged in the ticket).
/// Maps a role to the roles it *directly* inherits. [`RoleGraph::expand`] takes the actor's
/// directly-granted roles and returns their transitive closure.
///
/// ## Pinned semantics: inheritance is **additive-only** (allow-union).
/// A child role is a *super-set* of its parents' grants — inheritance only ever **adds** roles
/// to the actor's effective set, never subtracts. This is the conservative reading the ticket's
/// open decision flagged: it keeps a parent's `ALLOW` reachable from a child without letting a
/// child silently *remove* a parent grant (subtraction is expressed by an explicit `DENY` rule,
/// which still wins by precedence — see [`super::enforce`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleGraph {
    /// role → the roles it directly inherits.
    parents: BTreeMap<String, BTreeSet<String>>,
}

impl RoleGraph {
    /// An empty graph (no inheritance).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare that `role` directly inherits `parent` (builder). Self-edges are ignored.
    #[must_use]
    pub fn inherits(mut self, role: impl Into<String>, parent: impl Into<String>) -> Self {
        let role = role.into();
        let parent = parent.into();
        if role != parent {
            self.parents.entry(role).or_default().insert(parent);
        }
        self
    }

    /// The transitive closure of `roles` under inheritance (additive union). Cycle-safe: a
    /// visited set bounds the walk, so a `a→b→a` declaration terminates.
    #[must_use]
    pub fn expand(&self, roles: &BTreeSet<String>) -> BTreeSet<String> {
        let mut out: BTreeSet<String> = BTreeSet::new();
        let mut stack: Vec<String> = roles.iter().cloned().collect();
        while let Some(role) = stack.pop() {
            if !out.insert(role.clone()) {
                continue; // already expanded
            }
            if let Some(parents) = self.parents.get(&role) {
                for p in parents {
                    if !out.contains(p) {
                        stack.push(p.clone());
                    }
                }
            }
        }
        out
    }
}

/// One policy rule (RFD §8): grant/deny a [`VerbSet`] on a [`DriverGlob`]. Ordered within a
/// [`Policy`] — later rules refine earlier ones (the enforcer evaluates top-down and returns
/// the first matching rule's effectivity).
///
/// ## t57 — the richer axes (`who` / `where` / conditional)
/// Beyond the `what` (verbs) and driver scope, a rule now optionally narrows to a [`Subject`]
/// (`FOR role:admin`), a realm-scoped path [`ScopeGlob`] (`AT /members/alice/**`), and a
/// conditional grant [`Condition`] (`WHERE member_of('/directories/...')`). All three default to
/// the *unscoped* value, so a rule built the pre-t57 way (`Rule::allow`) is byte-for-byte the
/// same policy. A narrowed rule only matches when the resolved decision context satisfies every
/// axis — otherwise the effect falls to the fail-closed default-deny.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// Whether this rule allows or denies.
    pub effect: Effectivity,
    /// The verbs this rule governs.
    pub verbs: VerbSet,
    /// The driver scope (empty = every driver).
    pub driver: DriverGlob,
    /// Whether the rule was written as the bare `ALL` token (vs an explicit verb list). The
    /// irreversible-strictness rule (RFD §6/§10) uses this: a `ALLOW ALL` does **not** grant
    /// `REMOVE`/`CALL` (the irreversible classes) — those need an explicit `ALLOW REMOVE` /
    /// `ALLOW CALL`. An explicit `ALLOW SELECT,INSERT,REMOVE,CALL` *does* grant them.
    #[serde(default)]
    pub all_token: bool,
    /// **Who** the rule is for (t57). [`Subject::Anyone`] = the unscoped `FOR`-less rule.
    #[serde(default)]
    pub subject: Subject,
    /// The optional realm-scoped path scope (t57). `None` = every path (the `AT`-less rule).
    #[serde(default)]
    pub scope: Option<ScopeGlob>,
    /// The optional conditional grant (t57). [`Condition::Always`] = the `WHERE`-less rule.
    #[serde(default)]
    pub condition: Condition,
}

impl Rule {
    /// An allow rule over a verb set and driver glob (explicit verb list — `all_token = false`).
    /// The t57 axes default to unscoped (`FOR anyone`, every path, no condition).
    #[must_use]
    pub fn allow(verbs: VerbSet, driver: DriverGlob) -> Self {
        Rule {
            effect: Effectivity::Allow,
            verbs,
            driver,
            all_token: false,
            subject: Subject::Anyone,
            scope: None,
            condition: Condition::Always,
        }
    }

    /// A deny rule over a verb set and driver glob (t57 axes default to unscoped).
    #[must_use]
    pub fn deny(verbs: VerbSet, driver: DriverGlob) -> Self {
        Rule {
            effect: Effectivity::Deny,
            verbs,
            driver,
            all_token: false,
            subject: Subject::Anyone,
            scope: None,
            condition: Condition::Always,
        }
    }

    /// Mark this rule as having been written with the bare `ALL` token (builder).
    #[must_use]
    pub fn as_all_token(mut self) -> Self {
        self.all_token = true;
        self
    }

    /// Narrow this rule to a [`Subject`] (the `FOR` clause — builder, t57).
    #[must_use]
    pub fn for_subject(mut self, subject: Subject) -> Self {
        self.subject = subject;
        self
    }

    /// Narrow this rule to a realm-scoped path (the `AT` clause — builder, t57).
    #[must_use]
    pub fn scoped(mut self, scope: ScopeGlob) -> Self {
        self.scope = Some(scope);
        self
    }

    /// Attach a conditional grant (the `WHERE` clause — builder, t57).
    #[must_use]
    pub fn when(mut self, condition: Condition) -> Self {
        self.condition = condition;
        self
    }

    /// Whether this rule is a broad `ALL`-token grant (vs an explicit verb list). A broad
    /// `ALLOW ALL` is held back from the irreversible classes by the enforcer.
    #[must_use]
    pub fn is_broad_all(&self) -> bool {
        self.all_token
    }

    /// Whether this rule's verbs + driver glob match the given effect `(verb, driver, path)`,
    /// honoring the irreversible-strictness rule for a broad `ALL` allow.
    #[must_use]
    pub fn matches(&self, verb: Verb, driver: &str, path: &str) -> bool {
        if !self.driver.matches(driver, path) {
            return false;
        }
        if !self.verbs.contains(verb) {
            return false;
        }
        // Irreversible strictness (RFD §6/§10): a broad `ALLOW ALL` does NOT grant the
        // irreversible classes (REMOVE/CALL). A *deny* ALL still denies them (deny is never
        // weakened). So the hold-back applies only to an allow.
        if self.effect == Effectivity::Allow && self.is_broad_all() && verb.is_irreversible_class()
        {
            return false;
        }
        true
    }
}

/// A least-privilege policy (RFD §8/§10): a named, ordered list of [`Rule`]s plus the
/// fail-closed [`Effectivity`] default. The **may** layer over t13's **can** layer.
///
/// `Default` is `default-deny` with no rules — a handler bound to an empty/default policy
/// denies every effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// The policy name (the `/server/policies` row key).
    pub name: String,
    /// The ordered rules (later rules refine earlier).
    pub rules: Vec<Rule>,
    /// The fail-closed default applied when no rule matches an effect.
    pub default: Effectivity,
}

impl Default for Policy {
    /// The fail-closed default: no rules, default-DENY. An empty policy denies everything.
    fn default() -> Self {
        Policy {
            name: String::new(),
            rules: Vec::new(),
            default: Effectivity::Deny,
        }
    }
}

impl Policy {
    /// A named, empty, default-deny policy (denies every effect until a rule widens it).
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Policy {
            name: name.into(),
            ..Policy::default()
        }
    }

    /// Append a rule (builder).
    #[must_use]
    pub fn with_rule(mut self, rule: Rule) -> Self {
        self.rules.push(rule);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbset_all_contains_every_verb() {
        let all = VerbSet::all();
        for v in Verb::ALL {
            assert!(all.contains(v), "ALL must contain {}", v.label());
        }
        assert!(all.is_all());
        assert_eq!(all.verbs(), Verb::ALL.to_vec());
    }

    #[test]
    fn verbset_from_verbs_and_contains() {
        let s = VerbSet::from_verbs(&[Verb::Select, Verb::Insert]);
        assert!(s.contains(Verb::Select));
        assert!(s.contains(Verb::Insert));
        assert!(!s.contains(Verb::Remove));
        assert!(!s.is_all());
        assert!(!s.is_empty());
    }

    #[test]
    fn default_policy_is_deny_with_no_rules() {
        let p = Policy::default();
        assert_eq!(p.default, Effectivity::Deny);
        assert!(p.rules.is_empty());
    }

    #[test]
    fn driver_glob_matching() {
        assert!(DriverGlob::any().matches("mail", "/mail/inbox"));
        assert!(DriverGlob::new("mail").matches("mail", "/mail/inbox"));
        assert!(!DriverGlob::new("mail").matches("s3", "/s3/bucket"));
        assert!(DriverGlob::new("s3/*").matches("s3", "/s3/bucket/key"));
        assert!(!DriverGlob::new("s3/*").matches("s3", "/s3"));
        // Bare-driver target (no path): match against the driver id.
        assert!(DriverGlob::new("mail").matches("mail", ""));
    }

    #[test]
    fn verb_label_roundtrip() {
        for v in Verb::ALL {
            assert_eq!(Verb::from_label(v.label()), Some(v));
        }
        assert_eq!(Verb::from_label("BANANA"), None);
    }

    #[test]
    fn subject_label_roundtrips() {
        for s in [
            Subject::Anyone,
            Subject::User("u1".into()),
            Subject::Role("admin".into()),
            Subject::Group("eng".into()),
        ] {
            assert_eq!(Subject::from_label(&s.label()), Some(s));
        }
        assert_eq!(Subject::from_label("nope:"), None);
        assert_eq!(Subject::from_label("bogus:x"), None);
    }

    #[test]
    fn scope_glob_matches_within_its_realm_only() {
        let scope = ScopeGlob::parse("/members/alice/**").unwrap();
        assert_eq!(scope.realm(), Realm::Members);
        // Same realm + principal sub-tree ⇒ match.
        assert!(scope.matches_path("/members/alice/inbox"));
        assert!(scope.matches_path("/members/alice/inbox/deep/leaf"));
        // Same realm, different principal ⇒ no match.
        assert!(!scope.matches_path("/members/bob/inbox"));
        // Different realm ⇒ no match (the realm gate, decision P).
        assert!(!scope.matches_path("/projects/alice/inbox"));
        // A bare (self-realm) node ⇒ no match against a Members scope.
        assert!(!scope.matches_path("/mail/inbox"));
    }

    #[test]
    fn scope_glob_single_star_is_one_segment() {
        let scope = ScopeGlob::parse("/me/mail/*").unwrap();
        assert_eq!(scope.realm(), Realm::Me);
        assert!(scope.matches_path("/me/mail/inbox"));
        // `*` is exactly one segment — a deeper path does not match a single star.
        assert!(!scope.matches_path("/me/mail/inbox/x"));
        // A bare path anchors in the self realm, so `/mail/...` matches a `/me/...` scope.
        let bare = ScopeGlob::parse("/mail/*").unwrap();
        assert_eq!(bare.realm(), Realm::Me);
        assert!(bare.matches_path("/mail/inbox"));
    }

    #[test]
    fn scope_glob_round_trips_through_render() {
        for p in [
            "/members/alice/**",
            "/projects/acme/orders/*",
            "/me/mail/inbox",
        ] {
            let g = ScopeGlob::parse(p).unwrap();
            assert_eq!(g.render(), p);
        }
        assert!(ScopeGlob::parse("/").is_none());
        assert!(ScopeGlob::parse("").is_none());
    }

    #[test]
    fn condition_label_round_trips() {
        let c = Condition::MemberOf("/directories/google/groups/eng".into());
        assert_eq!(
            c.label().as_deref(),
            Some("member_of('/directories/google/groups/eng')")
        );
        assert_eq!(c.member_of_ref(), Some("/directories/google/groups/eng"));
        assert_eq!(Condition::Always.label(), None);
        assert!(Condition::Always.is_always());
    }

    #[test]
    fn role_graph_expand_is_transitive_and_cycle_safe() {
        let g = RoleGraph::new()
            .inherits("owner", "admin")
            .inherits("admin", "member")
            // a self-edge and a cycle must not loop forever.
            .inherits("member", "member")
            .inherits("member", "owner");
        let expanded = g.expand(&BTreeSet::from(["owner".to_string()]));
        assert!(expanded.contains("owner"));
        assert!(expanded.contains("admin"));
        assert!(expanded.contains("member"));
        assert_eq!(expanded.len(), 3);
    }

    #[test]
    fn richer_rule_serde_round_trips() {
        // The richer axes must survive a serde round-trip (the `/sys/policies` JSON shape).
        let rule = Rule::allow(VerbSet::one(Verb::Insert), DriverGlob::new("mail"))
            .for_subject(Subject::Role("admin".into()))
            .scoped(ScopeGlob::parse("/members/alice/**").unwrap())
            .when(Condition::MemberOf("/directories/x".into()));
        let json = serde_json::to_string(&rule).unwrap();
        let back: Rule = serde_json::from_str(&json).unwrap();
        assert_eq!(rule, back);
    }

    #[test]
    fn pre_t57_rule_deserializes_without_new_fields() {
        // A rule serialized before t57 (no subject/scope/condition keys) must rehydrate to the
        // unscoped defaults — `#[serde(default)]` is the forward-compat guarantee.
        let legacy = r#"{"effect":"allow","verbs":2,"driver":"mail","all_token":false}"#;
        let rule: Rule = serde_json::from_str(legacy).unwrap();
        assert_eq!(rule.subject, Subject::Anyone);
        assert_eq!(rule.scope, None);
        assert_eq!(rule.condition, Condition::Always);
    }
}
