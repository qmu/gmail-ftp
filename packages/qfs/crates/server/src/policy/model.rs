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

/// One policy rule (RFD §8): grant/deny a [`VerbSet`] on a [`DriverGlob`]. Ordered within a
/// [`Policy`] — later rules refine earlier ones (the enforcer evaluates top-down and returns
/// the first matching rule's effectivity).
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
}

impl Rule {
    /// An allow rule over a verb set and driver glob (explicit verb list — `all_token = false`).
    #[must_use]
    pub fn allow(verbs: VerbSet, driver: DriverGlob) -> Self {
        Rule {
            effect: Effectivity::Allow,
            verbs,
            driver,
            all_token: false,
        }
    }

    /// A deny rule over a verb set and driver glob.
    #[must_use]
    pub fn deny(verbs: VerbSet, driver: DriverGlob) -> Self {
        Rule {
            effect: Effectivity::Deny,
            verbs,
            driver,
            all_token: false,
        }
    }

    /// Mark this rule as having been written with the bare `ALL` token (builder).
    #[must_use]
    pub fn as_all_token(mut self) -> Self {
        self.all_token = true;
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
}
