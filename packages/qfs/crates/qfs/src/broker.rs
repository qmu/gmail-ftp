//! t66 (roadmap **M9** — Managed Team / §3.2/§3.3): the binary-side **team-connection provisioning +
//! brokered-USE** seam — where the pure brokering model ([`qfs_oauth`] `broker`) meets the real
//! envelope-encrypted store (t43) and the t81 shared-connection gate.
//!
//! ## What this module owns (the hermetic CORE)
//! - [`provision_team_connection`] — drive a [`Broker`] (the [`FixtureBroker`] in tests, the live qfs
//!   Cloud network broker in production behind the SAME trait) to mint a team-scoped token, then
//!   store it **envelope-encrypted** in the Project DB (t43) and record the ownership + brokering
//!   metadata rows (t81 `shared_connection` + t66 `broker_connection`). A **non-member is refused with
//!   no secret stored** — the broker returns no token, so nothing is sealed.
//! - [`provision_broker_client_registration`] — persist the broker's **client secret**
//!   envelope-encrypted at rest (the crown jewel goes through the SAME t43 envelope as any
//!   credential; it is never written in the clear).
//! - [`resolve_brokered_secret`] — the USE-time gate composition: a brokered connection's secret
//!   decrypts only after (1) the acting member belongs to the connection's TEAM (t66 team scope) AND
//!   (2) their t57/t81 actor-policy grants the connection's scope. Either gate failing refuses the
//!   bind **without decrypting** (default-deny — no secret crosses to an unauthorized actor).
//!
//! ## What is a documented SEAM (not in this repo, not claimed to work)
//! The **live qfs Cloud broker endpoint** is a network service that does not exist to test against.
//! It implements [`Broker`] over the network (holding the real upstream client registration and
//! exchanging for a real provider token scoped to the team); the binary's commit path
//! (`crates/qfs/src/commit.rs` `networked_credential`/`live_registry`) would call it exactly where it
//! calls the fixture here. This module proves the brokering DATA MODEL + provisioning + gates; it does
//! NOT ship a working qfs Cloud broker.
//!
//! ## Open product decision (FLAGGED, not baked in)
//! The brokering topology — does qfs Cloud hold the team refresh token **centrally**, or does each
//! tenant's **Project DB** hold the brokered token (sealed under t43)? — is managed-tier shaped. This
//! module implements the **tenant-Project-DB-at-rest** choice (it is the hermetically testable one)
//! and leaves the central-custody variant to the live-broker seam. The per-user-override precedence
//! over a team default (resolve.rs ladder) is likewise a managed-tier policy call to confirm in the PR.

use qfs_oauth::{Broker, BrokerError, BrokerTokenRequest, TeamId};
use qfs_secrets::{CredentialKey, OwnerScope, Secret, SecretError, Secrets};
use rusqlite::Connection;

use crate::secret_store::{db_record_broker_connection, db_share_connection, BrokerConnectionRow};
use crate::shared_connection::{resolve_shared_secret, SharedBindError};

/// Why provisioning a team connection failed — either the broker refused (no token minted) or the
/// at-rest store failed. Both are secret-free.
#[derive(Debug)]
pub enum ProvisionError {
    /// The broker refused to mint a token (unknown team / non-member / unoffered scope). **No secret
    /// was stored** — the credential never existed for the refused request (default-deny).
    Broker(BrokerError),
    /// The broker minted a token but sealing it / recording the metadata failed. Carries the
    /// secret-free [`SecretError`].
    Store(SecretError),
}

impl ProvisionError {
    /// A short, stable error code for structured surfaces / logs (secret-free).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            ProvisionError::Broker(e) => e.code(),
            ProvisionError::Store(e) => e.code(),
        }
    }
}

impl std::fmt::Display for ProvisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProvisionError::Broker(e) => write!(f, "{e}"),
            ProvisionError::Store(e) => write!(f, "{e}"),
        }
    }
}

/// Provision a **team connection** through the broker (t66 / M9).
///
/// The flow, in order (the membership gate runs FIRST, so a refusal never reaches the store):
/// 1. ask `broker` to mint a team-scoped token for `req` — a **non-member is refused here** with
///    [`ProvisionError::Broker`] and NOTHING is sealed (the secret never exists for them);
/// 2. seal the brokered token under the project DB's data-key via `store.put` (t43 envelope at rest),
///    keyed by `(driver, connection)` — the token belongs to the project, not a person;
/// 3. mark the connection **project-owned** (t81 `shared_connection`) at `realm_scope` so the
///    actor-policy USE gate applies, and record the **brokering provenance** (t66 `broker_connection`:
///    team, provider, the broker's PUBLIC client id, scope, who provisioned it).
///
/// Returns the secret-free [`BrokerConnectionRow`] describing what was provisioned (never the token).
///
/// # Errors
/// [`ProvisionError::Broker`] if the broker refuses (no secret stored); [`ProvisionError::Store`] on a
/// seal / DB failure.
pub fn provision_team_connection(
    broker: &dyn Broker,
    store: &dyn Secrets,
    conn: &Connection,
    driver: &str,
    connection: &str,
    req: &BrokerTokenRequest,
    realm_scope: &str,
) -> Result<BrokerConnectionRow, ProvisionError> {
    // 1. Broker the team-scoped token. A non-member / unknown team / unoffered scope refuses HERE,
    //    before any secret is sealed (default-deny — the credential never exists for them).
    let brokered = broker.broker_token(req).map_err(ProvisionError::Broker)?;

    // 2. Seal the brokered token at rest (t43 envelope). The token is moved into the store and never
    //    logged; it is the team's, keyed by (driver, connection).
    let key = credential_key(driver, connection).map_err(ProvisionError::Store)?;
    store
        .put(&key, brokered.token)
        .map_err(ProvisionError::Store)?;

    // 3. Record ownership (t81) + brokering provenance (t66) — selectors + metadata only.
    db_share_connection(conn, driver, connection, realm_scope, &req.member)
        .map_err(ProvisionError::Store)?;
    db_record_broker_connection(
        conn,
        driver,
        connection,
        brokered.grant.team.as_str(),
        &brokered.grant.provider,
        brokered.grant.client_id.as_str(),
        &brokered.grant.scope,
        &req.member,
    )
    .map_err(ProvisionError::Store)?;

    Ok(BrokerConnectionRow {
        team: brokered.grant.team.as_str().to_string(),
        provider: brokered.grant.provider,
        broker_client_id: brokered.grant.client_id.as_str().to_string(),
        scope: brokered.grant.scope,
        brokered_by: req.member.clone(),
        created_at: String::new(),
    })
}

/// Persist the broker's **client secret** envelope-encrypted at rest (t66 / M9). The broker client
/// secret is the crown jewel — it goes through the SAME t43 envelope as any credential, so a stolen
/// DB file holds only ciphertext. Keyed under a reserved `(driver, connection)` so it never collides
/// with a real connection's slot. The secret is moved into the store and never logged.
///
/// (In the managed topology the LIVE broker holds this secret; this is the at-rest mechanism wherever
/// it is held — the open custody decision is flagged in the module docs.)
///
/// # Errors
/// [`SecretError`] on a seal / DB failure (secret-free message).
pub fn provision_broker_client_registration(
    store: &dyn Secrets,
    provider: &str,
    client_secret: Secret,
) -> Result<(), SecretError> {
    let key = credential_key("__broker__", provider)?;
    store.put(&key, client_secret)
}

/// The USE-time **brokered-secret gate** (t66 / M9): resolve a brokered connection's secret only after
/// BOTH gates pass, decrypting behind them.
///
/// 1. **Team scope (t66):** the acting member must belong to the connection's `team` — a member of a
///    different team (or no team) is refused with [`BrokerError::NotAMember`] and the secret is NEVER
///    decrypted (a brokered token is the team's; cross-team use is default-deny).
/// 2. **Actor policy (t81):** the project-owned connection then runs the pure
///    [`resolve_shared_secret`] gate — `resolve_secret` (the decrypt) runs ONLY if the actor's policy
///    granted the connection's scope.
///
/// `resolve_secret` is the (impure) decrypt, passed as a `FnOnce` so it structurally cannot run before
/// both gates pass. Either gate failing means no secret crosses to an unauthorized actor.
///
/// # Errors
/// [`BrokeredUseError::WrongTeam`] if the member is not in the connection's team (no decrypt);
/// [`BrokeredUseError::Shared`] if the t81 actor-policy gate refuses or the decrypt fails.
pub fn resolve_brokered_secret<F>(
    member_teams: &[TeamId],
    connection_team: &TeamId,
    connection: &str,
    scope: &str,
    actor_granted: bool,
    resolve_secret: F,
) -> Result<Secret, BrokeredUseError>
where
    F: FnOnce() -> Result<Secret, SecretError>,
{
    // TEAM GATE BEFORE DECRYPT: a non-member of the connection's team is refused without touching the
    // secret (the brokered token is the team's — it is not transferable to another team).
    if !member_teams.contains(connection_team) {
        return Err(BrokeredUseError::WrongTeam(BrokerError::NotAMember {
            team: connection_team.as_str().to_string(),
            member: String::new(),
        }));
    }
    // ACTOR-POLICY GATE BEFORE DECRYPT (t81): only a granted actor reaches the decrypt.
    resolve_shared_secret(
        OwnerScope::Project,
        connection,
        scope,
        actor_granted,
        resolve_secret,
    )
    .map_err(BrokeredUseError::Shared)
}

/// Why USE of a brokered connection did not yield a secret — the team-scope gate refused (a non-member
/// of the team), or the t81 actor-policy gate refused / the decrypt failed. Both are secret-free; in
/// every refusal case the secret is NEVER decrypted.
#[derive(Debug)]
pub enum BrokeredUseError {
    /// The acting member is not in the connection's team — refused without decrypting (t66 team scope).
    WrongTeam(BrokerError),
    /// The t81 shared-connection gate refused (ungranted actor) or the decrypt failed.
    Shared(SharedBindError),
}

impl BrokeredUseError {
    /// A short, stable error code for structured surfaces / logs (secret-free).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            BrokeredUseError::WrongTeam(e) => e.code(),
            BrokeredUseError::Shared(e) => e.code(),
        }
    }
}

impl std::fmt::Display for BrokeredUseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrokeredUseError::WrongTeam(e) => write!(f, "{e}"),
            BrokeredUseError::Shared(e) => write!(f, "{e}"),
        }
    }
}

/// Build a `(driver, connection)` credential key, validating the connection name. An invalid name is
/// a secret-free [`SecretError::Backend`] (a connection name is metadata, never a credential) rather
/// than a panic — the no-`unwrap`/`expect` lib policy.
fn credential_key(driver: &str, connection: &str) -> Result<CredentialKey, SecretError> {
    use qfs_secrets::{ConnectionId, DriverId};
    let connection = ConnectionId::new(connection)
        .map_err(|e| SecretError::Backend(format!("invalid connection name: {e}")))?;
    Ok(CredentialKey::new(DriverId::new(driver), connection))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::cell::Cell;

    use qfs_oauth::{BrokerClientId, FixtureBroker};
    use qfs_secrets::Secret;

    use crate::secret_store::{db_get_broker_connection, db_get_shared_connection, SqliteSecrets};
    use qfs_store::{MemorySource, ProjectDb};

    const CLIENT_SECRET: &str = "broker_client_secret_LEAK_CANARY";

    fn team(s: &str) -> TeamId {
        TeamId::new(s).unwrap()
    }

    fn fixture() -> FixtureBroker {
        FixtureBroker::new(
            BrokerClientId::new("qfs-cloud-broker-google"),
            Secret::from(CLIENT_SECRET),
        )
        .with_team(
            team("acme"),
            ["alice@acme.co", "bob@acme.co"],
            ["drive.readonly"],
        )
        .with_team(team("beta"), ["carol@beta.co"], ["drive.readonly"])
    }

    /// A member's team connection is provisioned through the fixture broker: the token is sealed at
    /// rest (t43), and the ownership + brokering rows are recorded — all metadata, no token leaked.
    #[test]
    fn a_team_connection_is_provisioned_through_the_broker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.db");
        let open = || {
            ProjectDb::open(&qfs_store::FileSource::new(&path))
                .unwrap()
                .into_db()
                .into_connection()
        };
        let store = SqliteSecrets::open_or_init(open(), &Secret::from("pass")).unwrap();
        let meta = open();

        let broker = fixture();
        let req =
            BrokerTokenRequest::new(team("acme"), "alice@acme.co", "google", "drive.readonly");
        let summary = provision_team_connection(
            &broker,
            &store,
            &meta,
            "gdrive",
            "team",
            &req,
            "/projects/acme/**",
        )
        .unwrap();
        assert_eq!(summary.team, "acme");
        assert_eq!(summary.broker_client_id, "qfs-cloud-broker-google");

        // The ownership row (t81) marks it project-owned at the realm scope.
        let owner = db_get_shared_connection(&meta, "gdrive", "team").unwrap();
        assert_eq!(owner.scope, "/projects/acme/**");
        // The brokering row (t66) records the team binding.
        let brokered = db_get_broker_connection(&meta, "gdrive", "team").unwrap();
        assert_eq!(brokered.team, "acme");
        assert_eq!(brokered.provider, "google");

        // The brokered token is sealed at rest — readable by a granted actor, equal to a fresh mint.
        let expected = broker.broker_token(&req).unwrap();
        let key = credential_key("gdrive", "team").unwrap();
        let got = store.get(&key).unwrap();
        assert_eq!(got.expose_str(), expected.token.expose_str());
    }

    /// A non-member's provisioning attempt is refused by the broker and **nothing is stored** — no
    /// sealed token, no ownership row, no brokering row (default-deny: the secret never exists).
    #[test]
    fn a_non_member_provisioning_attempt_stores_no_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.db");
        let open = || {
            ProjectDb::open(&qfs_store::FileSource::new(&path))
                .unwrap()
                .into_db()
                .into_connection()
        };
        let store = SqliteSecrets::open_or_init(open(), &Secret::from("pass")).unwrap();
        let meta = open();

        let broker = fixture();
        // carol belongs to `beta`, not `acme`.
        let req =
            BrokerTokenRequest::new(team("acme"), "carol@beta.co", "google", "drive.readonly");
        let err = provision_team_connection(
            &broker,
            &store,
            &meta,
            "gdrive",
            "team",
            &req,
            "/projects/acme/**",
        )
        .unwrap_err();
        assert_eq!(err.code(), "broker_not_a_member");

        // NOTHING was persisted: no sealed credential, no ownership/brokering rows.
        let key = credential_key("gdrive", "team").unwrap();
        assert_eq!(store.get(&key).unwrap_err().code(), "secret_not_found");
        assert!(db_get_shared_connection(&meta, "gdrive", "team").is_none());
        assert!(db_get_broker_connection(&meta, "gdrive", "team").is_none());
    }

    /// The broker CLIENT SECRET is envelope-encrypted at rest: stored via the t43 envelope, its
    /// ciphertext column (peeked through a SEPARATE raw connection to the same DB file) never contains
    /// the plaintext.
    #[test]
    fn the_broker_client_secret_is_envelope_encrypted_at_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project.db");
        let conn = ProjectDb::open(&qfs_store::FileSource::new(&path))
            .unwrap()
            .into_db()
            .into_connection();
        let store = SqliteSecrets::open_or_init(conn, &Secret::from("pass")).unwrap();
        provision_broker_client_registration(&store, "google", Secret::from(CLIENT_SECRET))
            .unwrap();

        // The sealed value round-trips for the broker, but the ciphertext at rest is not the plaintext.
        let key = credential_key("__broker__", "google").unwrap();
        assert_eq!(store.get(&key).unwrap().expose_str(), Some(CLIENT_SECRET));

        // Peek the raw ciphertext column via a separate connection to the same file — never plaintext.
        let raw = Connection::open(&path).unwrap();
        let ct: Vec<u8> = raw
            .query_row(
                "SELECT ciphertext FROM secret_store WHERE driver = '__broker__'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !ct.windows(CLIENT_SECRET.len())
                .any(|w| w == CLIENT_SECRET.as_bytes()),
            "the broker client secret leaked into the ciphertext column"
        );
    }

    /// USE of a brokered connection: a member of the connection's team WITH a policy grant decrypts;
    /// a NON-member of the team is refused WITHOUT the secret ever being decrypted (no secret crosses
    /// to an unauthorized actor), and so is a team member WITHOUT the actor-policy grant.
    #[test]
    fn brokered_use_refuses_a_non_member_and_an_ungranted_member_before_decrypt() {
        const PLANTED: &str = "brokered:TEAM_TOKEN_LEAK_CANARY";
        let acme = team("acme");

        // (a) A team member with a grant decrypts (the resolver runs after both gates).
        let decrypted = Cell::new(false);
        let got = resolve_brokered_secret(
            std::slice::from_ref(&acme),
            &acme,
            "team",
            "/projects/acme/**",
            /* actor_granted = */ true,
            || {
                decrypted.set(true);
                Ok(Secret::from(PLANTED))
            },
        )
        .unwrap();
        assert!(decrypted.get());
        assert_eq!(got.expose_str(), Some(PLANTED));

        // (b) A NON-member of the team is refused — the resolver never runs (team gate before decrypt).
        let decrypted = Cell::new(false);
        let out = resolve_brokered_secret(
            &[team("beta")], // member of beta, not acme
            &acme,
            "team",
            "/projects/acme/**",
            true,
            || {
                decrypted.set(true);
                Ok(Secret::from(PLANTED))
            },
        );
        assert!(
            !decrypted.get(),
            "the team gate must refuse before any decrypt"
        );
        match out {
            Err(BrokeredUseError::WrongTeam(_)) => {}
            other => panic!("a non-member must be refused, got {other:?}"),
        }

        // (c) A team member WITHOUT the actor-policy grant is refused by the t81 gate before decrypt.
        let decrypted = Cell::new(false);
        let out = resolve_brokered_secret(
            std::slice::from_ref(&acme),
            &acme,
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
            "the actor-policy gate must refuse before any decrypt"
        );
        match out {
            Err(BrokeredUseError::Shared(SharedBindError::Refused(_))) => {}
            other => panic!("an ungranted member must be refused, got {other:?}"),
        }
    }

    /// End-to-end with the REAL envelope store: a brokered token exists at rest, yet a non-member of
    /// the team never receives it (the team gate refuses before `get`), while a granted team member
    /// does — the secret never surfaces to an unauthorized actor.
    #[test]
    fn the_brokered_secret_at_rest_never_surfaces_to_a_non_member() {
        let conn = ProjectDb::open(&MemorySource)
            .unwrap()
            .into_db()
            .into_connection();
        let store = SqliteSecrets::open_or_init(conn, &Secret::from("pass")).unwrap();
        let key = credential_key("gdrive", "team").unwrap();
        store
            .put(&key, Secret::from("brokered:REAL_AT_REST_CANARY"))
            .unwrap();
        let acme = team("acme");

        // A non-member of the team is refused; store.get is never called.
        let refused = resolve_brokered_secret(
            &[team("beta")],
            &acme,
            "team",
            "/projects/acme/**",
            true,
            || store.get(&key),
        );
        match refused {
            Err(BrokeredUseError::WrongTeam(_)) => {}
            other => panic!("a non-member must be refused, got {other:?}"),
        }
        // A granted team member decrypts the same stored secret.
        let got = resolve_brokered_secret(
            std::slice::from_ref(&acme),
            &acme,
            "team",
            "/projects/acme/**",
            true,
            || store.get(&key),
        )
        .unwrap();
        assert_eq!(got.expose_str(), Some("brokered:REAL_AT_REST_CANARY"));
    }
}
