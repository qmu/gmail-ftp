//! t67 (roadmap **M9** — Billing / §3.4): the binary-side **payment-provider SEAM** + the webhook
//! → plan-state apply path. Where the pure billing-tier model ([`qfs_identity::billing`]) meets the
//! real System DB (`/sys/billing`, [`crate::sys::SystemDbBackend`]) and a payment provider.
//!
//! ## What this module owns (the hermetic CORE — tested)
//! - [`PaymentProvider`] — the trait a payment provider implements (create a checkout session, read a
//!   subscription's current state). A [`StubPaymentProvider`] in-memory impl backs the tests + the
//!   no-vendor default; there is NO payment SDK in this repo.
//! - [`SubscriptionEvent`] — the owned, provider-agnostic DTO a `subscription.*` webhook decodes to
//!   ([`SubscriptionEvent::from_webhook_payload`]) — secret-free (a team id + tier/status labels +
//!   period end; never a card, a token, or a provider key).
//! - [`apply_subscription_event`] — apply a (verified) event to `/sys/billing` plan state through the
//!   System DB, **idempotently** (the t67 dedup ledger). This is the at-least-once webhook update:
//!   a free→paid event flips the [`qfs_identity::Capability::TeamConnections`] gate; a replayed event
//!   is a no-op.
//!
//! ## What is a documented SEAM (not in this repo, not claimed to work)
//! ### The PAYMENT PROVIDER is an OPEN PRODUCT DECISION — flagged, not baked in
//! The roadmap explicitly leaves the payment provider (Stripe, Paddle, …), its fee model, and its
//! tax/invoicing scope as a **business decision** (roadmap §3.4 / M9). This module ships the
//! [`PaymentProvider`] SEAM + a stub; it does NOT pull a vendor SDK, charge a card, or claim a working
//! billing integration. The LIVE provider impl would implement [`PaymentProvider`] over the network
//! (riding `crates/qfs/src/transport.rs`'s `HttpExchange` pattern, the provider API key resolved BY
//! HANDLE from the envelope vault, t43 — never inlined / logged) and is named as the one open
//! decision in the PR.
//!
//! ### The webhook TRANSPORT rides `qfs-watchtower` (already built)
//! A provider's `subscription.changed` webhook arrives at the existing `qfs-watchtower`
//! [`WebhookBinding`](qfs_watchtower::WebhookBinding), which **HMAC-SHA256-verifies** the request
//! against the provider's signing secret (resolved BY HANDLE, constant-time compared, never logged)
//! before any byte reaches this module. So no payment secret crosses into [`apply_subscription_event`]
//! — only the already-verified, decoded plan labels. The verified body decodes via
//! [`SubscriptionEvent::from_webhook_payload`]; the apply is idempotent on the provider event id
//! (at-least-once delivery, reusing watchtower's dedup posture at the billing ledger).
//!
//! ## Safety floor (§3 purity / RFD §10 redaction + default-deny)
//! - A payment secret NEVER lands in `/sys/billing` or a log — the plan row is metadata only and the
//!   provider key / signing secret live envelope-encrypted in the vault.
//! - An unknown/garbled tier or status decodes to the FREE floor through the pure model — fail-closed
//!   toward the lower tier (an unpaid/unknown team never gains paid entitlements).

use qfs_identity::{BillingPlan, Capability, SubscriptionStatus, Tier};
use serde::Deserialize;

use crate::sys::SystemDbBackend;

/// A request to start a checkout for a team's paid subscription (the provider mints a hosted
/// checkout session the operator completes). Secret-free — a team id + the desired tier only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutRequest {
    /// The team buying the subscription.
    pub team_id: String,
    /// The tier being purchased (today only [`Tier::PaidTeam`] is a paid checkout).
    pub tier: Tier,
}

/// A provider-hosted checkout session — the URL the operator visits to pay, plus the provider's
/// session id (an opaque handle, not a secret). The provider charges the card on its hosted page;
/// qfs never sees the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutSession {
    /// The provider's opaque session id (echoed back on the completion webhook).
    pub session_id: String,
    /// The hosted checkout URL the operator completes payment at.
    pub url: String,
}

/// A provider-agnostic subscription event — what a `subscription.*` webhook (or a status read)
/// decodes to. **Secret-free:** a provider event id (for dedup), the team, the tier/status labels,
/// and the optional period end. NEVER a card, a token, or a provider key.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SubscriptionEvent {
    /// The provider's unique event id — the at-least-once DEDUP key (`billing_events` PK). A replayed
    /// event shares this id and is applied at most once.
    #[serde(rename = "id")]
    pub event_id: String,
    /// The team this subscription belongs to.
    pub team_id: String,
    /// The tier label (`free-individual` / `paid-team`; an unknown value fails closed to free).
    pub tier: String,
    /// The subscription status label (`active` / `past-due` / `canceled` / `inactive`; an unknown
    /// value fails closed to non-active).
    pub status: String,
    /// The current billing period end, if the provider reported one (metadata; does not affect the
    /// gate).
    #[serde(default)]
    pub current_period_end: Option<String>,
}

impl SubscriptionEvent {
    /// Decode a (HMAC-verified) provider webhook body into an owned, secret-free event. The body is
    /// the provider's JSON `subscription.*` payload, normalized to the provider-agnostic shape.
    ///
    /// # Errors
    /// [`BillingError::MalformedPayload`] if the body is not the expected JSON shape (no panic).
    pub fn from_webhook_payload(body: &[u8]) -> Result<Self, BillingError> {
        serde_json::from_slice(body).map_err(|e| BillingError::MalformedPayload {
            reason: e.to_string(),
        })
    }

    /// The pure [`BillingPlan`] this event records — decoded through the fail-closed model (an
    /// unknown tier/status resolves to the free floor).
    #[must_use]
    pub fn plan(&self) -> BillingPlan {
        BillingPlan::decode(&self.tier, &self.status)
    }
}

/// The payment-provider SEAM (t67). A provider implements this; the LIVE impl is a flagged open
/// decision (no vendor baked in). The trait is intentionally tiny — checkout-session create + a
/// subscription read — because the SOURCE OF TRUTH for entitlements is the `/sys/billing` plan state,
/// not a live provider call (the provider drives plan state via webhooks).
pub trait PaymentProvider: Send + Sync {
    /// Create a hosted checkout session for `req` (the operator completes payment on the provider's
    /// page; the provider then delivers a `subscription.active` webhook).
    ///
    /// # Errors
    /// [`BillingError`] on a provider/transport failure.
    fn create_checkout_session(
        &self,
        req: &CheckoutRequest,
    ) -> Result<CheckoutSession, BillingError>;

    /// Read a team's current subscription state from the provider (a reconciliation read, e.g. to
    /// repair a missed webhook).
    ///
    /// # Errors
    /// [`BillingError`] on a provider/transport failure.
    fn fetch_subscription(&self, team_id: &str) -> Result<SubscriptionEvent, BillingError>;
}

/// A structured, **secret-free** billing error (AI-consumable). Never carries a card, a provider key,
/// or a webhook signing secret.
#[derive(Debug, thiserror::Error)]
pub enum BillingError {
    /// A webhook body / provider response was not the expected shape.
    #[error("malformed billing payload: {reason}")]
    MalformedPayload {
        /// A secret-free reason (a serde decode message — never a payload value).
        reason: String,
    },
    /// The provider seam is not configured (the no-vendor default refuses rather than guesses).
    #[error("no payment provider is configured (the provider is an open product decision)")]
    NoProvider,
    /// A provider/transport failure (secret-free message).
    #[error("payment provider error: {0}")]
    Provider(String),
}

/// Apply a (HMAC-verified, decoded) [`SubscriptionEvent`] to `/sys/billing` plan state through the
/// System DB, **idempotently** (the t67 at-least-once webhook update). Delegates to
/// [`SystemDbBackend::apply_provider_event`], which dedups on the provider event id inside one
/// transaction (a replayed event is a no-op). Returns `true` when the event was applied, `false` for
/// a deduped replay.
///
/// No payment secret crosses into this function — the event is already verified + decoded, carrying
/// only secret-free plan labels.
///
/// # Errors
/// [`BillingError::Provider`] on a System-DB failure (secret-free).
pub fn apply_subscription_event(
    backend: &SystemDbBackend,
    event: &SubscriptionEvent,
) -> Result<bool, BillingError> {
    backend
        .apply_provider_event(
            &event.event_id,
            &event.team_id,
            &event.tier,
            &event.status,
            event.current_period_end.as_deref(),
        )
        .map_err(|e| BillingError::Provider(e.to_string()))
}

/// The entitlement gate, resolved against LIVE `/sys/billing` plan state (t67): may `team_id`'s
/// recorded plan exercise `capability`? Reads the plan through the fail-closed
/// [`SystemDbBackend::get_billing_plan`] (a missing/unknown/lapsed plan is the free floor) and
/// applies the pure [`BillingPlan::gate`]. This is the binary's chokepoint that turns "what tier is
/// recorded" into "is this paid-only feature permitted" — default-deny toward free.
///
/// # Errors
/// [`qfs_identity::EntitlementDenied`] when the team's effective tier does not entitle `capability`.
pub fn gate_team_capability(
    backend: &SystemDbBackend,
    team_id: &str,
    capability: Capability,
) -> Result<(), qfs_identity::EntitlementDenied> {
    backend.get_billing_plan(team_id).gate(capability)
}

/// An in-memory [`PaymentProvider`] stub for tests + the no-vendor default (NO network, NO SDK, NO
/// real charge). It records the subscription state it is told to and echoes it back — enough to drive
/// the webhook → plan-state path hermetically without committing to a vendor.
#[derive(Debug, Default)]
pub struct StubPaymentProvider {
    /// A monotonically-incrementing counter so each checkout session id is distinct.
    counter: std::sync::atomic::AtomicU64,
}

impl StubPaymentProvider {
    /// Construct the stub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl PaymentProvider for StubPaymentProvider {
    fn create_checkout_session(
        &self,
        req: &CheckoutRequest,
    ) -> Result<CheckoutSession, BillingError> {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // A deterministic, secret-free fake session — no card, no real provider call.
        Ok(CheckoutSession {
            session_id: format!("stub-session-{n}"),
            url: format!("https://payments.example/checkout/{}/{}", req.team_id, n),
        })
    }

    fn fetch_subscription(&self, team_id: &str) -> Result<SubscriptionEvent, BillingError> {
        // The stub has no recorded subscription; it reports the free floor (fail-closed).
        Ok(SubscriptionEvent {
            event_id: format!("stub-read-{team_id}"),
            team_id: team_id.to_string(),
            tier: Tier::FreeIndividual.as_str().to_string(),
            status: SubscriptionStatus::Inactive.as_str().to_string(),
            current_period_end: None,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use qfs_store::{FileSource, SystemDb};
    use tempfile::TempDir;

    fn backend() -> (TempDir, SystemDbBackend) {
        let dir = TempDir::new().unwrap();
        let sys = SystemDb::open(&FileSource::new(dir.path().join("system.db")))
            .unwrap()
            .into_db()
            .into_connection();
        (dir, SystemDbBackend::new(sys, None))
    }

    #[test]
    fn stub_checkout_session_is_secret_free_and_distinct() {
        let provider = StubPaymentProvider::new();
        let req = CheckoutRequest {
            team_id: "team-acme".to_string(),
            tier: Tier::PaidTeam,
        };
        let s1 = provider.create_checkout_session(&req).unwrap();
        let s2 = provider.create_checkout_session(&req).unwrap();
        assert_ne!(s1.session_id, s2.session_id, "each session id is distinct");
        assert!(s1.url.contains("team-acme"));
        // The stub never invents a card / token / secret.
        let dump = format!("{s1:?}{s2:?}").to_lowercase();
        assert!(!dump.contains("card") && !dump.contains("token") && !dump.contains("secret"));
    }

    #[test]
    fn webhook_payload_decodes_to_a_secret_free_event() {
        let body = br#"{"id":"evt-9","team_id":"team-acme","tier":"paid-team","status":"active","current_period_end":"2026-12-31"}"#;
        let ev = SubscriptionEvent::from_webhook_payload(body).unwrap();
        assert_eq!(ev.event_id, "evt-9");
        assert_eq!(ev.team_id, "team-acme");
        assert_eq!(ev.plan(), BillingPlan::paid_team());
        // current_period_end is optional (a payload without it still decodes).
        let minimal = br#"{"id":"e","team_id":"t","tier":"free-individual","status":"inactive"}"#;
        assert!(SubscriptionEvent::from_webhook_payload(minimal).is_ok());
        // A malformed body is a structured error, never a panic.
        assert!(SubscriptionEvent::from_webhook_payload(b"not json").is_err());
    }

    #[test]
    fn webhook_apply_flips_the_gate_and_dedups_replays() {
        let (_d, be) = backend();
        // Free floor before any event.
        assert!(gate_team_capability(&be, "team-acme", Capability::TeamConnections).is_err());

        // A free→paid event flips the gate to ALLOW.
        let upgrade = SubscriptionEvent {
            event_id: "evt-1".to_string(),
            team_id: "team-acme".to_string(),
            tier: "paid-team".to_string(),
            status: "active".to_string(),
            current_period_end: Some("2026-12-31".to_string()),
        };
        assert!(apply_subscription_event(&be, &upgrade).unwrap(), "applied");
        assert!(gate_team_capability(&be, "team-acme", Capability::TeamConnections).is_ok());

        // A REPLAY of evt-1 is a deduped no-op; the gate stays ALLOW (no double-apply).
        assert!(
            !apply_subscription_event(&be, &upgrade).unwrap(),
            "deduped replay"
        );
        assert!(gate_team_capability(&be, "team-acme", Capability::TeamConnections).is_ok());

        // A new cancellation event fails the gate closed to free.
        let cancel = SubscriptionEvent {
            event_id: "evt-2".to_string(),
            team_id: "team-acme".to_string(),
            tier: "free-individual".to_string(),
            status: "canceled".to_string(),
            current_period_end: None,
        };
        assert!(apply_subscription_event(&be, &cancel).unwrap());
        assert!(gate_team_capability(&be, "team-acme", Capability::TeamConnections).is_err());
    }
}
