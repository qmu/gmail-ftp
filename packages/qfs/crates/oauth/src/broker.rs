//! t66 (roadmap **M9** — Managed Team / §3.2/§3.3): the **OAuth-brokering domain model** — the
//! pure request/grant types + the security gates behind qfs Cloud brokering a token to a *team's*
//! connection, so members act *as the team* without ever registering a personal OAuth client.
//!
//! ## The brokering model (decision: qfs Cloud is the OAuth client)
//! In the managed tier qfs Cloud holds **one** broker OAuth client registration per upstream
//! provider (the broker `client_id` + the `client_secret` — the crown jewel). Instead of every
//! self-hosted deployment registering its own client with Google/GitHub/Slack and minting its own
//! tokens, the broker mints a **team-scoped** token and hands it to the team's connection. A member
//! then USES that brokered connection *as the team*, bounded by the t57/t81 actor-policy — never by
//! who holds the token (§3.3 two-layer identity: the connection is the upstream authority, the actor
//! is the human).
//!
//! ## What is here (the hermetic CORE) vs. the live broker (a documented SEAM)
//! The live qfs Cloud broker is a **network service that does not exist to test against**, so this
//! module ships only the testable core:
//! - the **request/grant types** ([`BrokerTokenRequest`], [`BrokeredGrant`], [`BrokeredToken`]);
//! - the [`Broker`] trait — the SEAM the live qfs Cloud broker endpoint implements over the network
//!   (the binary's commit path calls it; production swaps a network impl behind the same trait —
//!   that impl is NOT in this repo and is NOT claimed to work);
//! - an in-memory [`FixtureBroker`] — the **reference + hermetic** impl the tests (and the binary's
//!   provisioning tests) drive, so the whole brokering model is exercised with no network/credentials;
//! - the **security gates** ([`Broker::broker_token`] refuses a non-member; [`assert_team_scope`]
//!   refuses cross-team replay) that make brokering safe by construction.
//!
//! ## Secret discipline (RFD §10)
//! Two secrets exist here and BOTH are carried only inside the redacting, zeroized [`Secret`]:
//! - the broker **client secret** — held by the broker, **never** placed in a [`BrokeredGrant`], a
//!   log, or any value that crosses to a team or a member (it is the broker's alone);
//! - the **brokered token** — bound to a team; it rides inside [`BrokeredToken`] next to a
//!   secret-free [`BrokeredGrant`] descriptor (team / provider / scope / `client_id`) so everything a
//!   log or `/sys/connections` view touches is metadata, never a credential.
//!
//! The brokered token is **derived from** the broker client secret + the team + provider + scope (a
//! one-way `sha256`), so a fixture token is reproducibly bound to its team WITHOUT ever exposing the
//! client secret — the same property the live broker gives by minting a real upstream token scoped to
//! the team.

use std::collections::{BTreeSet, HashMap};

use qfs_crypto_core::sha256_hex;
use qfs_secrets::Secret;
use serde::{Deserialize, Serialize};

/// A **team** identity (the managed-tier project a brokered connection belongs to) — secret-free
/// metadata, safe to log / serialize / surface in `/sys/connections`. Validated non-empty so a
/// brokered grant can never be bound to an empty team (which would defeat the team-scope gate).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TeamId(String);

impl TeamId {
    /// Construct a team id, rejecting an empty string.
    ///
    /// # Errors
    /// [`BrokerError::EmptyTeam`] if `id` is empty.
    pub fn new(id: impl Into<String>) -> Result<Self, BrokerError> {
        let id = id.into();
        if id.is_empty() {
            return Err(BrokerError::EmptyTeam);
        }
        Ok(Self(id))
    }

    /// The team id as a string slice (metadata, never a secret).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for TeamId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The broker's public OAuth `client_id` (the qfs Cloud broker client registered with the upstream
/// provider). Public metadata — NOT the client secret — so it is safe to record next to a connection.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BrokerClientId(String);

impl BrokerClientId {
    /// Construct a broker client id from any text (the upstream provider mints it; we only carry it).
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The client id as a string slice (public metadata).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A request to the broker for a **team-scoped** token (the provisioning input). All metadata —
/// secret-free — so it is safe to log: it names WHO (`member`) wants to act as WHICH team (`team`)
/// against WHICH upstream (`provider`) at WHICH `scope`. The broker decides whether to honour it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerTokenRequest {
    /// The team the brokered token is requested for.
    pub team: TeamId,
    /// The acting member (their t45 human handle / federated identity, t56). The broker brokers a
    /// token ONLY to a member of `team` — a non-member is refused with NO token minted.
    pub member: String,
    /// The upstream provider key (e.g. `google`, `github`, `slack`) the broker holds a client for.
    pub provider: String,
    /// The upstream scope requested (e.g. `drive.readonly`). A §10 hint, never a token.
    pub scope: String,
}

impl BrokerTokenRequest {
    /// Assemble a request (all fields are secret-free metadata).
    #[must_use]
    pub fn new(
        team: TeamId,
        member: impl Into<String>,
        provider: impl Into<String>,
        scope: impl Into<String>,
    ) -> Self {
        Self {
            team,
            member: member.into(),
            provider: provider.into(),
            scope: scope.into(),
        }
    }
}

/// The **secret-free descriptor** of a brokered grant — team / provider / scope / broker `client_id`.
/// This is everything a log, an audit row, or a `/sys/connections` projection may see; the token it
/// describes lives separately in [`BrokeredToken::token`]. Bound to a [`TeamId`] so the team-scope
/// gate ([`assert_team_scope`]) can reject a grant minted for another team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokeredGrant {
    /// The team this grant is scoped to. The load-bearing binding — a token is the team's, not a
    /// person's, and cannot be replayed for a different team.
    pub team: TeamId,
    /// The upstream provider the brokered token authenticates against.
    pub provider: String,
    /// The upstream scope the brokered token carries (metadata, never a token).
    pub scope: String,
    /// The broker's PUBLIC client id (not the secret) — records which broker registration minted it.
    pub client_id: BrokerClientId,
}

/// A brokered token: the secret-free [`BrokeredGrant`] descriptor + the team-scoped token itself
/// (carried only inside the redacting, zeroized [`Secret`]). Never `Clone` (it owns a `Secret`) and
/// its `Debug` shows only the grant — the `Secret`'s own `Debug` redacts the bytes.
#[derive(Debug)]
pub struct BrokeredToken {
    /// The secret-free descriptor (team / provider / scope / client id).
    pub grant: BrokeredGrant,
    /// The team-scoped token. Bound to `grant.team`; the bind path stores it envelope-encrypted.
    pub token: Secret,
}

/// Why the broker refused a request — structured + **secret-free** (a team / member / scope are
/// metadata; a refusal NEVER carries a token or the client secret). AI-actionable: each names exactly
/// what was missing so the operator knows the remedy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrokerError {
    /// A [`TeamId`] was constructed empty (a brokered grant must name a real team).
    #[error("a team id must not be empty")]
    EmptyTeam,
    /// The broker holds no registration for the requested team (nothing to broker against).
    #[error("no broker registration for team '{team}'")]
    UnknownTeam {
        /// The team that has no broker registration.
        team: String,
    },
    /// The acting member is not a member of the team — **fail closed**: no token is minted, the
    /// secret never exists for them. The remedy is a team membership (t55/t56), not a token.
    #[error(
        "'{member}' is not a member of team '{team}' — a brokered token is the team's and is \
         minted only for a member (no token was issued)"
    )]
    NotAMember {
        /// The team requested.
        team: String,
        /// The non-member who was refused.
        member: String,
    },
    /// The broker registration does not offer the requested upstream scope for this team — the broker
    /// only brokers the scopes its client is registered for (default-deny on an unrequested scope).
    #[error("team '{team}' broker does not offer scope '{scope}' for provider '{provider}'")]
    ScopeNotOffered {
        /// The team requested.
        team: String,
        /// The provider requested.
        provider: String,
        /// The scope that is not offered.
        scope: String,
    },
    /// A brokered grant minted for one team was presented for another — a cross-team replay. **Fail
    /// closed**: a team's token is usable only by that team ([`assert_team_scope`]).
    #[error("brokered grant is scoped to team '{presented}', not the expected team '{expected}'")]
    WrongTeam {
        /// The team the caller expected the grant to be for.
        expected: String,
        /// The team the grant is actually bound to.
        presented: String,
    },
}

impl BrokerError {
    /// A short, stable error code for structured/JSON surfaces + AI feedback (mirrors the other
    /// secret-free taxonomies, e.g. [`crate::RegistrationError::code`]).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            BrokerError::EmptyTeam => "broker_empty_team",
            BrokerError::UnknownTeam { .. } => "broker_unknown_team",
            BrokerError::NotAMember { .. } => "broker_not_a_member",
            BrokerError::ScopeNotOffered { .. } => "broker_scope_not_offered",
            BrokerError::WrongTeam { .. } => "broker_wrong_team",
        }
    }
}

/// The **brokering SEAM**: mint a team-scoped token for a [`BrokerTokenRequest`].
///
/// The live qfs Cloud broker implements this over the network (it holds the real upstream client
/// registration and exchanges for a real provider token scoped to the team) — that impl is a
/// documented seam, NOT in this repo. [`FixtureBroker`] is the in-memory reference impl the whole
/// test suite (and the binary's provisioning path) drives hermetically.
///
/// The contract every impl must uphold (the security floor):
/// - a **non-member** of the team is refused with [`BrokerError::NotAMember`] and **no token is
///   minted** (default-deny — the secret never exists for them);
/// - the returned [`BrokeredToken::grant`] is bound to the request's [`TeamId`] (team-scoped), so a
///   later [`assert_team_scope`] can reject a cross-team replay;
/// - the broker's **client secret never appears** in the grant, the token descriptor, or any error.
pub trait Broker {
    /// Broker a team-scoped token for `req`, or refuse with a secret-free [`BrokerError`].
    ///
    /// # Errors
    /// [`BrokerError`] per the contract above (unknown team / non-member / unoffered scope).
    fn broker_token(&self, req: &BrokerTokenRequest) -> Result<BrokeredToken, BrokerError>;
}

/// The team-scope **replay gate**: a [`BrokeredGrant`] is usable only by the team it was minted for.
/// The binary calls this before binding a brokered connection so a grant captured for team A can
/// never authorize team B (the headline "a brokered token is scoped to the team" invariant).
///
/// # Errors
/// [`BrokerError::WrongTeam`] when `grant.team != expected`.
pub fn assert_team_scope(grant: &BrokeredGrant, expected: &TeamId) -> Result<(), BrokerError> {
    if &grant.team != expected {
        return Err(BrokerError::WrongTeam {
            expected: expected.as_str().to_string(),
            presented: grant.team.as_str().to_string(),
        });
    }
    Ok(())
}

/// One team's registration inside the [`FixtureBroker`]: who may act as the team and which scopes the
/// broker offers it. Secret-free (the token is derived on demand; the client secret lives on the
/// broker, not here).
#[derive(Debug, Clone, Default)]
struct TeamRegistration {
    /// The members allowed to be brokered a token as this team (a non-member is refused).
    members: BTreeSet<String>,
    /// The upstream scopes the broker client is registered to offer this team (default-deny outside).
    offered_scopes: BTreeSet<String>,
}

/// An **in-memory reference broker** — the hermetic stand-in for the live qfs Cloud broker. It holds
/// the broker `client_id` + `client_secret` (the crown jewel, only ever inside a [`Secret`]) and a
/// per-team membership/scope registration, and brokers a **team-scoped** token derived one-way from
/// the client secret so the token is reproducibly bound to its team WITHOUT exposing the secret.
///
/// This is a real, shippable type (not `#[cfg(test)]`): it is the testable core the binary's
/// provisioning path drives, and the place the security gates are proven. Production replaces it with
/// a network [`Broker`] behind the same trait — nothing else changes.
pub struct FixtureBroker {
    /// The broker's PUBLIC client id (safe to record next to a connection).
    client_id: BrokerClientId,
    /// The broker's client SECRET — held by the broker alone, never exposed in a grant / log / error.
    client_secret: Secret,
    /// Per-team membership + offered scopes.
    teams: HashMap<TeamId, TeamRegistration>,
}

impl FixtureBroker {
    /// Create a broker holding the registration `client_id` + `client_secret`. The secret is moved
    /// into a [`Secret`] and never leaves the broker.
    #[must_use]
    pub fn new(client_id: BrokerClientId, client_secret: Secret) -> Self {
        Self {
            client_id,
            client_secret,
            teams: HashMap::new(),
        }
    }

    /// Register a team with its `members` and the upstream `scopes` the broker offers it (builder).
    /// Re-registering the same team replaces its membership/scope set.
    #[must_use]
    pub fn with_team(
        mut self,
        team: TeamId,
        members: impl IntoIterator<Item = impl Into<String>>,
        scopes: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let reg = TeamRegistration {
            members: members.into_iter().map(Into::into).collect(),
            offered_scopes: scopes.into_iter().map(Into::into).collect(),
        };
        self.teams.insert(team, reg);
        self
    }

    /// The broker's public client id (metadata).
    #[must_use]
    pub fn client_id(&self) -> &BrokerClientId {
        &self.client_id
    }

    /// Derive the **team-scoped** token one-way from the client secret + team + provider + scope. A
    /// pure `sha256` over the binding so the token is reproducible AND bound to its team, while the
    /// client secret is never recoverable from (or present in) the output. In production the live
    /// broker instead returns a real upstream token scoped to the team — the BINDING is the same.
    fn mint_team_token(&self, req: &BrokerTokenRequest) -> Secret {
        let material = format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}",
            sha256_hex(self.client_secret.expose()),
            req.team.as_str(),
            req.provider,
            req.scope,
        );
        Secret::from(format!("brokered:{}", sha256_hex(material.as_bytes())))
    }
}

impl Broker for FixtureBroker {
    fn broker_token(&self, req: &BrokerTokenRequest) -> Result<BrokeredToken, BrokerError> {
        // Unknown team: nothing to broker against (fail closed).
        let reg = self
            .teams
            .get(&req.team)
            .ok_or_else(|| BrokerError::UnknownTeam {
                team: req.team.as_str().to_string(),
            })?;

        // MEMBERSHIP GATE: a non-member is refused BEFORE any token is minted (default-deny — the
        // secret never exists for them).
        if !reg.members.contains(&req.member) {
            return Err(BrokerError::NotAMember {
                team: req.team.as_str().to_string(),
                member: req.member.clone(),
            });
        }

        // SCOPE GATE: the broker only brokers the scopes its client is registered to offer.
        if !reg.offered_scopes.contains(&req.scope) {
            return Err(BrokerError::ScopeNotOffered {
                team: req.team.as_str().to_string(),
                provider: req.provider.clone(),
                scope: req.scope.clone(),
            });
        }

        // Mint a team-scoped token + its secret-free descriptor.
        let token = self.mint_team_token(req);
        let grant = BrokeredGrant {
            team: req.team.clone(),
            provider: req.provider.clone(),
            scope: req.scope.clone(),
            client_id: self.client_id.clone(),
        };
        Ok(BrokeredToken { grant, token })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT_SECRET: &str = "broker_client_secret_LEAK_CANARY";

    fn team(s: &str) -> TeamId {
        TeamId::new(s).unwrap()
    }

    fn broker() -> FixtureBroker {
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

    #[test]
    fn a_member_is_brokered_a_team_scoped_token() {
        let b = broker();
        let req =
            BrokerTokenRequest::new(team("acme"), "alice@acme.co", "google", "drive.readonly");
        let brokered = b.broker_token(&req).unwrap();
        // The grant is bound to the requesting team.
        assert_eq!(brokered.grant.team, team("acme"));
        assert_eq!(brokered.grant.client_id.as_str(), "qfs-cloud-broker-google");
        // The token exists and is non-empty, but is NOT the client secret.
        assert!(!brokered.token.is_empty());
        assert_ne!(brokered.token.expose_str(), Some(CLIENT_SECRET));
        // The team-scope gate accepts the matching team and rejects another.
        assert!(assert_team_scope(&brokered.grant, &team("acme")).is_ok());
        assert_eq!(
            assert_team_scope(&brokered.grant, &team("beta"))
                .unwrap_err()
                .code(),
            "broker_wrong_team"
        );
    }

    #[test]
    fn a_brokered_token_is_scoped_to_the_team() {
        // The same member+provider+scope for two different teams yields DIFFERENT tokens and grants
        // bound to their own team — a token is the team's, not transferable across teams.
        let b = FixtureBroker::new(BrokerClientId::new("c"), Secret::from(CLIENT_SECRET))
            .with_team(team("acme"), ["alice@acme.co"], ["drive.readonly"])
            .with_team(team("beta"), ["alice@acme.co"], ["drive.readonly"]);
        let a = b
            .broker_token(&BrokerTokenRequest::new(
                team("acme"),
                "alice@acme.co",
                "google",
                "drive.readonly",
            ))
            .unwrap();
        let c = b
            .broker_token(&BrokerTokenRequest::new(
                team("beta"),
                "alice@acme.co",
                "google",
                "drive.readonly",
            ))
            .unwrap();
        assert_ne!(
            a.token.expose_str(),
            c.token.expose_str(),
            "a brokered token must differ per team"
        );
        assert_eq!(a.grant.team, team("acme"));
        assert_eq!(c.grant.team, team("beta"));
        // A grant for team acme cannot be presented as team beta (cross-team replay refused).
        assert_eq!(
            assert_team_scope(&a.grant, &team("beta"))
                .unwrap_err()
                .code(),
            "broker_wrong_team"
        );
    }

    #[test]
    fn a_non_member_is_refused_with_no_token() {
        let b = broker();
        // carol belongs to `beta`, not `acme`.
        let err = b
            .broker_token(&BrokerTokenRequest::new(
                team("acme"),
                "carol@beta.co",
                "google",
                "drive.readonly",
            ))
            .unwrap_err();
        assert_eq!(err.code(), "broker_not_a_member");
        // The refusal is secret-free — neither the client secret nor any token marker leaks.
        let rendered = format!("{err:?} {err}");
        assert!(!rendered.contains(CLIENT_SECRET));
        for forbidden in ["token=", "brokered:", "secret_value"] {
            assert!(!rendered.contains(forbidden), "leaked `{forbidden}`");
        }
    }

    #[test]
    fn an_unoffered_scope_is_refused() {
        let b = broker();
        let err = b
            .broker_token(&BrokerTokenRequest::new(
                team("acme"),
                "alice@acme.co",
                "google",
                "gmail.modify", // not offered
            ))
            .unwrap_err();
        assert_eq!(err.code(), "broker_scope_not_offered");
    }

    #[test]
    fn an_unknown_team_is_refused() {
        let b = broker();
        let err = b
            .broker_token(&BrokerTokenRequest::new(
                team("ghost"),
                "alice@acme.co",
                "google",
                "drive.readonly",
            ))
            .unwrap_err();
        assert_eq!(err.code(), "broker_unknown_team");
    }

    #[test]
    fn the_client_secret_never_appears_in_a_grant_or_token() {
        // The grant descriptor + the derived token are both free of the client secret material.
        let b = broker();
        let brokered = b
            .broker_token(&BrokerTokenRequest::new(
                team("acme"),
                "alice@acme.co",
                "google",
                "drive.readonly",
            ))
            .unwrap();
        let grant_json = serde_json::to_string(&brokered.grant).unwrap();
        assert!(!grant_json.contains(CLIENT_SECRET));
        // The token is derived one-way; the secret is not recoverable from (or equal to) it.
        let exposed = brokered.token.expose_str().unwrap();
        assert!(!exposed.contains(CLIENT_SECRET));
    }

    #[test]
    fn an_empty_team_id_is_rejected() {
        assert_eq!(TeamId::new("").unwrap_err().code(), "broker_empty_team");
    }
}
