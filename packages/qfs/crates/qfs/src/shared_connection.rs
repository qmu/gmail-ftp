//! t81 (roadmap **M5** — decision U / §3.3): the binary-side **shared-connection bind resolver** —
//! the commit-time seam that gates USE of a project/team-owned connection on the acting member's
//! actor-policy BEFORE the credential is decrypted.
//!
//! ## Where this sits (the §3 purity invariant)
//! A `Plan` carries a connection *selector*, never the secret. At commit/bind time the binary maps
//! the selector to a stored credential and resolves it lazily. t54 already gates a *cloud* bind on
//! sign-in + recorded consent. t81 adds the **team-sharing** gate, structurally identical: when the
//! selected connection is project-owned (`owner_scope = project`, a row in the project DB's
//! `shared_connection` table), the bind is allowed only if the acting member's t57 actor-policy
//! grants them the connection's realm scope. A user-owned connection is entirely unaffected.
//!
//! ## Two layers, kept apart (decision note: dep-direction)
//! - The **actor-policy decision** lives in `qfs-server` ([`qfs_mcp::evaluate_shared_use`], reached
//!   through the same `qfs-mcp` re-export window the binary already uses for `Policy` — no forbidden
//!   direct `qfs-server` edge). It answers "is this actor granted this connection's scope?" and
//!   yields the `actor_granted` boolean.
//! - The **bind gate** is the PURE leaf [`qfs_secrets::shared_use_gate`]: given the ownership label
//!   and `actor_granted`, it returns `Ok` (bind may resolve the secret) or a secret-free
//!   [`SharedUseError`](qfs_secrets::SharedUseError) (refuse — the secret is NEVER decrypted).
//!
//! [`resolve_shared_secret`] composes the two so the secret resolver (the decrypt) runs **only**
//! after a passing gate — the load-bearing "gated BEFORE decrypt" guarantee. The ownership read +
//! the policy evaluation are metadata-only and passphrase-free; no token is touched until the gate
//! passes.

use qfs_secrets::{shared_use_gate, OwnerScope, Secret, SecretError, SharedUseError};

/// Why a project-owned connection's bind did not yield a secret — either the actor-policy gate
/// **refused** USE (the secret was never decrypted), or the gate passed but the underlying secret
/// resolution failed. Both are secret-free.
#[derive(Debug)]
pub enum SharedBindError {
    /// The actor-policy gate refused USE of the project-owned connection (default-deny). The secret
    /// was NOT resolved — this is the "gated before decrypt" outcome.
    Refused(SharedUseError),
    /// The gate passed but resolving the secret failed (missing/locked credential). Carries the
    /// secret-free [`SecretError`].
    Secret(SecretError),
}

impl SharedBindError {
    /// A short, stable error code for structured surfaces / logs (secret-free).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            SharedBindError::Refused(e) => e.code(),
            SharedBindError::Secret(e) => e.code(),
        }
    }
}

impl std::fmt::Display for SharedBindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SharedBindError::Refused(e) => write!(f, "{e}"),
            SharedBindError::Secret(e) => write!(f, "{e}"),
        }
    }
}

/// Resolve the credential for a connection of ownership `owner`, **gating the decrypt** on the
/// shared-connection USE policy (t81).
///
/// The pure [`shared_use_gate`] runs FIRST over `(owner, connection, scope, actor_granted)`:
/// - a **user-owned** connection ([`OwnerScope::Me`]) is ungated — `resolve_secret` runs as before;
/// - a **project-owned** connection ([`OwnerScope::Project`]) runs `resolve_secret` ONLY when
///   `actor_granted` is true; otherwise it returns [`SharedBindError::Refused`] **without invoking
///   `resolve_secret`** — so the secret is never decrypted for an ungranted member (default-deny).
///
/// `resolve_secret` is the (impure) decrypt — typically `|| store.get(&key)`. Passing it as a
/// `FnOnce` is what makes the ordering guarantee structural: the closure cannot run before the gate
/// because it is moved into this call and only invoked on the `Ok` branch.
///
/// # Errors
/// [`SharedBindError::Refused`] when the gate denies a project-owned connection; otherwise the
/// result of `resolve_secret`, mapped to [`SharedBindError::Secret`].
pub fn resolve_shared_secret<F>(
    owner: OwnerScope,
    connection: &str,
    scope: &str,
    actor_granted: bool,
    resolve_secret: F,
) -> Result<Secret, SharedBindError>
where
    F: FnOnce() -> Result<Secret, SecretError>,
{
    // GATE BEFORE DECRYPT: refuse (without touching the secret) when the member is not granted.
    shared_use_gate(owner, connection, scope, actor_granted).map_err(SharedBindError::Refused)?;
    resolve_secret().map_err(SharedBindError::Secret)
}

// ---------------------------------------------------------------------------------------------
// Production wiring: the live bind path consults the gate so it is never inert.
// ---------------------------------------------------------------------------------------------

/// The commit-time gate the live registry consults before binding a credential (t81). Returns
/// `true` to allow the bind, `false` to refuse it (the driver is then left UNREGISTERED — fail
/// closed — exactly like t54's cloud consent gate).
///
/// - A **user-owned** connection (no `shared_connection` row) is never gated here ⇒ `true`. This
///   keeps every existing single-operator flow unchanged (the registry is empty by default).
/// - A **project-owned** connection is gated on the acting operator's actor-policy: it is allowed
///   only if the operator is signed in AND the project's stored policy grants their actor the
///   connection's recorded scope ([`operator_granted_scope`]). Any resolution gap (no config home,
///   no signed-in operator, no granting policy) fails closed.
///
/// Best-effort + passphrase-free: it reads only metadata (the ownership row, the operator identity,
/// the policy grants), never a token, BEFORE any decrypt.
#[must_use]
pub fn bind_allowed(driver: &str, connection: &str) -> bool {
    // Read the ownership row from the project DB (passphrase-free — it holds no key material).
    let Some(proj) = crate::store::open_project_db().ok().flatten() else {
        // No project DB ⇒ no shared connections recorded ⇒ nothing is project-owned ⇒ ungated.
        return true;
    };
    let conn = proj.into_db().into_connection();
    let Some(row) = crate::secret_store::db_get_shared_connection(&conn, driver, connection) else {
        // User-owned: the team-sharing gate does not apply.
        return true;
    };

    // Project-owned: compute the actor-policy grant and run the PURE bind gate. A denial leaves the
    // driver unregistered; the refusal reason is logged secret-free so the operator sees WHY.
    let actor_granted = operator_granted_scope(&row.scope);
    match shared_use_gate(OwnerScope::Project, connection, &row.scope, actor_granted) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                target: "qfs::shared",
                "shared connection '{driver}/{connection}' not bound: {} ({})",
                e,
                e.code()
            );
            false
        }
    }
}

/// Whether the signed-in operator's actor-policy grants them `scope` (t81). Resolves the sole
/// operator (sessions are not yet wired into the one-shot CLI — presence of exactly one identity is
/// the proxy, the same convention t54 uses), loads the stored `/sys/policies` grants into a
/// [`Policy`](qfs_mcp::Policy), and evaluates the shared-use gate over `scope`.
///
/// Fail-closed everywhere: no config home, no signed-in operator, an unreadable policy table, or no
/// granting rule all read as **not granted**. Reads metadata only (an identity's existence, the
/// policy grant rows) — never a token.
#[must_use]
fn operator_granted_scope(scope: &str) -> bool {
    use qfs_identity::{IdentityStore, SoleUser};
    use qfs_mcp::{evaluate_shared_use, DecisionContext};

    // The acting operator: the sole signed-up identity (fail closed if none / many / unreadable).
    let Ok(store) = crate::identity::open_identity_store() else {
        return false;
    };
    let actor = match store.sole_user() {
        // The human handle (primary email) is the actor id a project policy is authored against.
        Ok(SoleUser::One(user)) => user.primary_email,
        _ => return false,
    };

    // The project's stored grants, rehydrated into a Policy and evaluated for this operator.
    let Some(policy) = load_sys_policy() else {
        return false;
    };
    let ctx = DecisionContext::for_user(actor);
    evaluate_shared_use(&policy, &ctx, scope).is_allow()
}

/// Load the host's `/sys/policies` grants (System DB) into a [`Policy`](qfs_mcp::Policy) for the
/// shared-use gate. Each stored `(allow, target)` row becomes one ALLOW rule scoped by `target`
/// (when it parses as a realm path) — the use gate ignores the verb axis, so the rule's verbs are
/// the broad set. `None` if the System DB is unavailable. Secret-free (the policy table holds no
/// credentials).
fn load_sys_policy() -> Option<qfs_mcp::Policy> {
    let sys = crate::store::open_system_db().ok().flatten()?;
    let conn = sys.into_db().into_connection();
    let mut stmt = conn
        .prepare("SELECT allow, target FROM sys_policies ORDER BY id")
        .ok()?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
            ))
        })
        .ok()?;
    let pairs: Vec<(Option<String>, Option<String>)> = rows.filter_map(Result::ok).collect();
    Some(policy_from_sys_rows("sys", &pairs))
}

/// Build a [`Policy`](qfs_mcp::Policy) from stored `(allow, target)` `/sys/policies` rows for the
/// shared-use gate. Each row becomes an `ALLOW` rule whose realm scope is `target` (when `target`
/// parses as a realm path); a non-realm target yields an unscoped (broad) grant. The shared-use gate
/// consults only the subject/scope/condition axes, so the rule's verbs are the broad set and the
/// stored `allow` verb string is not parsed here. Pure — no I/O — so it is unit-testable.
fn policy_from_sys_rows(name: &str, rows: &[(Option<String>, Option<String>)]) -> qfs_mcp::Policy {
    use qfs_mcp::{DriverGlob, Policy, Rule, ScopeGlob, VerbSet};

    let mut policy = Policy::new(name);
    for (_allow, target) in rows {
        let mut rule = Rule::allow(VerbSet::all(), DriverGlob::any());
        if let Some(t) = target {
            if let Some(scope) = ScopeGlob::parse(t) {
                rule = rule.scoped(scope);
            }
        }
        policy = policy.with_rule(rule);
    }
    policy
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::cell::Cell;

    use qfs_secrets::{ConnectionId, CredentialKey, DriverId, Secrets};

    const PLANTED: &str = "ghp_TEAM_SECRET_LEAK_CANARY_42";

    /// The headline ordering guarantee: a project-owned connection with NO actor grant refuses the
    /// bind WITHOUT ever invoking the secret resolver (the decrypt). Proven by a closure that flips
    /// a flag — it must stay false on the refusal path.
    #[test]
    fn ungranted_project_connection_never_invokes_the_secret_resolver() {
        let decrypted = Cell::new(false);
        let out = resolve_shared_secret(
            OwnerScope::Project,
            "team",
            "/projects/acme/**",
            /* actor_granted = */ false,
            || {
                decrypted.set(true);
                Ok(Secret::from(PLANTED))
            },
        );
        assert!(
            !decrypted.get(),
            "the secret resolver (decrypt) MUST NOT run on a refusal"
        );
        match out {
            Err(SharedBindError::Refused(e)) => {
                assert_eq!(e.code(), "shared_connection_not_granted");
                // The refusal is secret-free.
                assert!(!format!("{e}").contains(PLANTED));
            }
            other => panic!("expected a Refused gate denial, got {other:?}"),
        }
    }

    /// A granted member DOES get the secret (the resolver runs after the passing gate).
    #[test]
    fn granted_project_connection_resolves_the_secret_after_the_gate() {
        let decrypted = Cell::new(false);
        let out = resolve_shared_secret(
            OwnerScope::Project,
            "team",
            "/projects/acme/**",
            /* actor_granted = */ true,
            || {
                decrypted.set(true);
                Ok(Secret::from(PLANTED))
            },
        );
        assert!(decrypted.get(), "a passing gate runs the resolver");
        assert_eq!(out.unwrap().expose_str(), Some(PLANTED));
    }

    /// A user-owned connection is unaffected: the resolver runs regardless of the (irrelevant)
    /// grant boolean — the existing single-operator path is untouched.
    #[test]
    fn user_owned_connection_is_unaffected_by_the_gate() {
        for granted in [false, true] {
            let decrypted = Cell::new(false);
            let out = resolve_shared_secret(OwnerScope::Me, "mine", "/me/**", granted, || {
                decrypted.set(true);
                Ok(Secret::from(PLANTED))
            });
            assert!(decrypted.get(), "a user-owned bind always resolves");
            assert_eq!(out.unwrap().expose_str(), Some(PLANTED));
        }
    }

    /// End-to-end with the REAL envelope-encrypted store: the secret exists at rest, yet an
    /// ungranted actor never receives it (the gate refuses before `get`), while a granted actor
    /// does. Proves the secret never surfaces to an unauthorized actor.
    #[test]
    fn the_encrypted_secret_never_surfaces_to_an_unauthorized_actor() {
        use crate::secret_store::SqliteSecrets;
        use qfs_store::{MemorySource, ProjectDb};

        let conn = ProjectDb::open(&MemorySource)
            .unwrap()
            .into_db()
            .into_connection();
        let store = SqliteSecrets::open_or_init(conn, &Secret::from("pass")).unwrap();
        let key = CredentialKey::new(DriverId::new("github"), ConnectionId::new("team").unwrap());
        store.put(&key, Secret::from(PLANTED)).unwrap();

        // Ungranted member ⇒ refused, and the resolver (store.get) is never reached.
        let refused = resolve_shared_secret(
            OwnerScope::Project,
            "team",
            "/projects/acme/**",
            false,
            || store.get(&key),
        );
        match refused {
            Err(SharedBindError::Refused(_)) => {}
            other => panic!("an ungranted actor must be refused, got {other:?}"),
        }
        // Granted member ⇒ the same stored secret decrypts.
        let got = resolve_shared_secret(
            OwnerScope::Project,
            "team",
            "/projects/acme/**",
            true,
            || store.get(&key),
        )
        .unwrap();
        assert_eq!(got.expose_str(), Some(PLANTED));
    }

    /// The production `/sys/policies` rehydration grants a covering realm scope and denies a
    /// non-covering / cross-realm one (the gate logic the live bind path runs).
    #[test]
    fn sys_policy_rehydration_grants_only_a_covering_scope() {
        use qfs_mcp::{evaluate_shared_use, DecisionContext};

        // A team grant for /projects/acme/** covers a connection scoped there.
        let policy = policy_from_sys_rows(
            "sys",
            &[(Some("ALL".into()), Some("/projects/acme/**".into()))],
        );
        let ctx = DecisionContext::for_user("a@b.com");
        assert!(evaluate_shared_use(&policy, &ctx, "/projects/acme/connections/github").is_allow());

        // A grant in a different realm does NOT cover a /projects connection (fail closed).
        let other =
            policy_from_sys_rows("sys", &[(Some("ALL".into()), Some("/members/a/**".into()))]);
        assert!(!evaluate_shared_use(&other, &ctx, "/projects/acme/connections/github").is_allow());

        // No policy rows at all ⇒ default-deny.
        let empty = policy_from_sys_rows("sys", &[]);
        assert!(!evaluate_shared_use(&empty, &ctx, "/projects/acme/connections/github").is_allow());
    }
}
