//! The **inbound event normalizer** (RFD-0001 §8) — the genuinely-hard, genuinely-**wasm-required**
//! part of the Slack driver. [`parse_event`] takes the Slack Events-API HTTP envelope (headers +
//! raw body + signing secret) and returns an owned [`SlackInbound`]: either the `url_verification`
//! challenge to echo, or a normalized [`SlackEvent`] the server's trigger bus (E7) consumes.
//!
//! ## Pure, no I/O — so it is wasm32-safe (the wasm subset)
//! [`parse_event`] performs **no I/O**: it reads the headers, recomputes the HMAC over the exact
//! bytes, compares constant-time, and parses the JSON. It holds no token, opens no socket, and does
//! not touch `qfs-runtime`/tokio — so it (and the introspective `Driver` surface) compiles for
//! `wasm32-unknown-unknown` (the Workers `WEBHOOK` ingress, RFD §8). It is unit-testable from
//! fixtures (no live Slack).
//!
//! ## Signature verification (RFD §10 replay defense)
//! Slack signs each delivery with `v0=<hex>` = `HMAC-SHA256(signing_secret, "v0:" + timestamp +
//! ":" + body)`. [`parse_event`]:
//! 1. rejects a missing/garbled `X-Slack-Signature` or `X-Slack-Request-Timestamp`;
//! 2. rejects a **stale** timestamp (skew beyond [`MAX_SKEW_SECS`]) — a replay of an old, validly
//!    signed delivery cannot pass;
//! 3. recomputes the HMAC and compares it **constant-time** ([`crate::hmac::constant_time_eq`]) —
//!    a tampered body or wrong secret fails without a timing oracle.
//!
//! ## Idempotency / dedupe (RFD §6)
//! Slack delivers events **at-least-once**; [`SlackEvent::event_id`] carries Slack's `event_id` so
//! the server trigger bus dedupes (events are *facts*; the plan they fire must be idempotent).

use crate::hmac::{constant_time_eq, hex_lower, hmac_sha256};

/// The maximum tolerated clock skew between the `X-Slack-Request-Timestamp` and `now` (seconds).
/// Slack's own recommendation is 5 minutes; a delivery older than this is rejected as a replay.
pub const MAX_SKEW_SECS: i64 = 60 * 5;

/// The Slack signature version prefix (the only version this driver verifies).
pub const SIG_VERSION: &str = "v0";

/// The owned, vendor-free headers [`parse_event`] reads — just the two Slack signs with plus an
/// optional retry hint. Built by the ingress (E7) from the real HTTP headers; no vendor type
/// crosses. Header lookups are case-insensitive at construction by the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct EventHeaders {
    /// `X-Slack-Signature` (`v0=<hex>`).
    pub signature: Option<String>,
    /// `X-Slack-Request-Timestamp` (unix seconds, as a string).
    pub timestamp: Option<String>,
    /// `X-Slack-Retry-Num` — Slack's at-least-once retry counter (a dedupe/observability hint).
    pub retry_num: Option<u32>,
}

impl EventHeaders {
    /// Build the headers from the signature + timestamp (the two required to verify).
    #[must_use]
    pub fn new(signature: impl Into<String>, timestamp: impl Into<String>) -> Self {
        Self {
            signature: Some(signature.into()),
            timestamp: Some(timestamp.into()),
            retry_num: None,
        }
    }

    /// Builder: attach the at-least-once retry counter.
    #[must_use]
    pub fn with_retry_num(mut self, n: u32) -> Self {
        self.retry_num = Some(n);
        self
    }
}

/// The normalized kind of an inbound event — the closed set this driver recognizes (RFD §9 enums).
/// An unrecognized `type` becomes [`SlackEventKind::Other`] carrying the raw type string (a fact
/// the trigger bus may still route), never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlackEventKind {
    /// A `message` posted to a channel/DM.
    Message,
    /// A `reaction_added` to a message.
    ReactionAdded,
    /// An `app_mention` (the bot was @-mentioned).
    AppMention,
    /// A `file_shared` event.
    FileShared,
    /// Any other event type — the raw `type` string is preserved.
    Other(String),
}

impl SlackEventKind {
    /// Map a Slack event `type` string onto the normalized kind.
    #[must_use]
    pub fn from_type(ty: &str) -> Self {
        match ty {
            "message" => SlackEventKind::Message,
            "reaction_added" => SlackEventKind::ReactionAdded,
            "app_mention" => SlackEventKind::AppMention,
            "file_shared" => SlackEventKind::FileShared,
            other => SlackEventKind::Other(other.to_string()),
        }
    }
}

/// One normalized inbound Slack event — the owned DTO the server's trigger bus consumes (RFD §8).
/// No Slack/vendor type crosses; the original envelope is retained as `raw` for fields this
/// normalizer does not surface, but routing/dedupe use only the typed fields.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SlackEvent {
    /// The normalized event kind.
    pub kind: SlackEventKind,
    /// Slack's `event_id` (the at-least-once dedupe key, RFD §6).
    pub event_id: Option<String>,
    /// The channel the event occurred in, if any.
    pub channel: Option<String>,
    /// The message `ts` (or `item.ts` for a reaction), if any.
    pub ts: Option<String>,
    /// The user who triggered the event, if any.
    pub user: Option<String>,
    /// For `reaction_added`, the reaction emoji name (without colons).
    pub reaction: Option<String>,
    /// The team/workspace id the event belongs to (echoes the envelope `team_id`).
    pub team_id: Option<String>,
    /// The original envelope, for fields not surfaced above (the trigger bus may inspect it).
    pub raw: serde_json::Value,
}

/// The result of [`parse_event`]: either the one-time URL-verification challenge to echo back, or
/// a normalized event delivery (RFD §8).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum SlackInbound {
    /// A `url_verification` request — the server echoes `challenge` back verbatim (the only reply
    /// Slack wants), no plan fires.
    UrlVerification {
        /// The challenge string to echo.
        challenge: String,
    },
    /// A normalized `event_callback` delivery the trigger bus routes.
    Event(SlackEvent),
    /// A **signature-valid** delivery whose top-level envelope `type` is neither `url_verification`
    /// nor `event_callback` (e.g. a future Slack control envelope). Carries the raw envelope + its
    /// `type` so the ingress can ack it (Slack expects a 200) and log it **without** the driver
    /// fabricating a synthetic `Event` from an envelope that has no inner `event` shape (Architect
    /// carry-over: keep the unhandled case structurally distinct from a real normalized event).
    Unhandled {
        /// The top-level envelope `type` string.
        envelope_type: String,
        /// The raw envelope, for the ingress to inspect/log.
        raw: serde_json::Value,
    },
}

/// Why [`parse_event`] rejected an inbound delivery — secret-free and structured (the signing
/// secret and the computed/expected HMAC never appear).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EventError {
    /// A required signing header (`X-Slack-Signature` / `X-Slack-Request-Timestamp`) was absent or
    /// malformed.
    #[error("missing or malformed signing header: {0}")]
    MissingHeader(&'static str),

    /// The `X-Slack-Request-Timestamp` was outside the allowed skew window — a replay of an old
    /// (validly signed) delivery is rejected here (RFD §10).
    #[error("request timestamp is outside the {MAX_SKEW_SECS}s skew window (possible replay)")]
    StaleTimestamp,

    /// The recomputed HMAC did not match the supplied `X-Slack-Signature` (tampered body or wrong
    /// signing secret). The compare was constant-time; no signature material is in the message.
    #[error("signature verification failed")]
    BadSignature,

    /// The body was not valid JSON, or did not carry the Events-API envelope shape.
    #[error("malformed event envelope: {0}")]
    MalformedEnvelope(&'static str),
}

impl EventError {
    /// A short, stable machine code for structured surfaces.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            EventError::MissingHeader(_) => "slack_event_missing_header",
            EventError::StaleTimestamp => "slack_event_stale_timestamp",
            EventError::BadSignature => "slack_event_bad_signature",
            EventError::MalformedEnvelope(_) => "slack_event_malformed_envelope",
        }
    }
}

/// Verify and normalize an inbound Slack Events-API delivery (RFD §8) — **pure, no I/O**.
///
/// `headers` carries the `X-Slack-Signature` + `X-Slack-Request-Timestamp`; `body` is the **exact**
/// raw request bytes (the signature is over the verbatim body — re-serializing would break it);
/// `signing_secret` is the workspace signing secret resolved at the edge; `now_unix` is the
/// caller's current unix time (injected so the function stays pure and deterministically testable).
///
/// # Errors
/// [`EventError`] on a missing header, a stale timestamp (replay), a bad signature, or a malformed
/// envelope. A bad signature is detected with a **constant-time** compare.
pub fn parse_event(
    headers: &EventHeaders,
    body: &[u8],
    signing_secret: &str,
    now_unix: i64,
) -> Result<SlackInbound, EventError> {
    verify_signature(headers, body, signing_secret, now_unix)?;

    let envelope: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| EventError::MalformedEnvelope("body was not valid JSON"))?;
    let ty = envelope
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or(EventError::MalformedEnvelope("envelope has no `type`"))?;

    match ty {
        // The one-time handshake: echo the challenge, fire no plan.
        "url_verification" => {
            let challenge = envelope
                .get("challenge")
                .and_then(serde_json::Value::as_str)
                .ok_or(EventError::MalformedEnvelope(
                    "url_verification has no `challenge`",
                ))?
                .to_string();
            Ok(SlackInbound::UrlVerification { challenge })
        }
        // A real event delivery.
        "event_callback" => {
            let inner = envelope.get("event").ok_or(EventError::MalformedEnvelope(
                "event_callback has no `event`",
            ))?;
            Ok(SlackInbound::Event(normalize(&envelope, inner)))
        }
        // Any other top-level type is surfaced as a distinct Unhandled fact (no panic, and NOT a
        // fabricated Event): the ingress acks it (Slack wants a 200) and logs it, but the trigger
        // bus is not handed a synthetic event built from an envelope with no inner `event` shape.
        other => Ok(SlackInbound::Unhandled {
            envelope_type: other.to_string(),
            raw: envelope.clone(),
        }),
    }
}

/// Verify the `v0` HMAC signature + timestamp freshness. Split out so the verification contract is
/// independently testable (a tampered body, a stale ts, a missing header).
///
/// # Errors
/// [`EventError`] per the rules in [`parse_event`].
pub fn verify_signature(
    headers: &EventHeaders,
    body: &[u8],
    signing_secret: &str,
    now_unix: i64,
) -> Result<(), EventError> {
    let sig = headers
        .signature
        .as_deref()
        .ok_or(EventError::MissingHeader("X-Slack-Signature"))?;
    let ts_str = headers
        .timestamp
        .as_deref()
        .ok_or(EventError::MissingHeader("X-Slack-Request-Timestamp"))?;
    let ts: i64 = ts_str
        .parse()
        .map_err(|_| EventError::MissingHeader("X-Slack-Request-Timestamp"))?;

    // (2) replay defense: reject a delivery whose timestamp is too far from now (either side).
    if (now_unix - ts).abs() > MAX_SKEW_SECS {
        return Err(EventError::StaleTimestamp);
    }

    // (3) recompute the HMAC over "v0:<ts>:<body>" and compare constant-time.
    let mut basestring = Vec::with_capacity(SIG_VERSION.len() + ts_str.len() + body.len() + 2);
    basestring.extend_from_slice(SIG_VERSION.as_bytes());
    basestring.push(b':');
    basestring.extend_from_slice(ts_str.as_bytes());
    basestring.push(b':');
    basestring.extend_from_slice(body);

    let tag = hmac_sha256(signing_secret.as_bytes(), &basestring);
    let expected = format!("{SIG_VERSION}={}", hex_lower(&tag));

    if constant_time_eq(expected.as_bytes(), sig.as_bytes()) {
        Ok(())
    } else {
        Err(EventError::BadSignature)
    }
}

/// Normalize an Events-API `event` object (and its envelope) into the owned [`SlackEvent`]. Reads
/// the well-known fields per event type; an unknown type still produces an event carrying the raw.
fn normalize(envelope: &serde_json::Value, event: &serde_json::Value) -> SlackEvent {
    let ty = event
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let kind = SlackEventKind::from_type(ty);

    // `reaction_added` nests its target under `item`; message/app_mention/file_shared are flat.
    let (channel, ts) = match &kind {
        SlackEventKind::ReactionAdded => {
            let item = event.get("item");
            (
                item.and_then(|i| i.get("channel"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                item.and_then(|i| i.get("ts"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            )
        }
        _ => (
            event
                .get("channel")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            event
                .get("ts")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        ),
    };

    SlackEvent {
        kind,
        event_id: envelope
            .get("event_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        channel,
        ts,
        user: event
            .get("user")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        reaction: event
            .get("reaction")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        team_id: envelope
            .get("team_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        raw: envelope.clone(),
    }
}
