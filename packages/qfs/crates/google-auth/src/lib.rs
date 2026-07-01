//! `qfs-google-auth` — the **shared Google OAuth2 + multi-account auth base** (RFD-0001
//! §5/§10, t19): the substrate the Gmail (t20), Drive (t21), and Analytics (t41) drivers
//! authenticate through. One Google "Desktop" OAuth client, a loopback consent flow, token
//! exchange + transparent refresh, per-account refresh-token storage via the t27 credential
//! store, and a reusable authenticated API client — all over the t18 thin HTTP seam, with
//! **no heavy vendor SDK** (RFD §171, owned DTOs only).
//!
//! ## What this crate provides
//! - [`OAuthClient`] — the thin OAuth2 endpoints client: [`OAuthClient::build_auth_url`]
//!   (`access_type=offline` + `prompt=consent` so a refresh token is reliably returned, plus
//!   `include_granted_scopes=true` so per-driver consents accumulate into one shared token),
//!   [`OAuthClient::exchange_code`], [`OAuthClient::refresh_access_token`],
//!   [`OAuthClient::fetch_profile_email`]. **Scope-agnostic**: the consuming driver passes its
//!   minimum scope set (least privilege).
//! - [`authorize`] (native-only) — the loopback consent flow: binds `127.0.0.1:0`, advertises
//!   the redirect URI as **`http://localhost:<port>`** (the load-bearing detail — Desktop
//!   clients stall on silent consent with the loopback IP), captures `code`, persists the
//!   refresh token under `google:<email>:refresh_token`.
//! - [`TokenSource`] + [`StoredTokenSource`] — the reusable bearer provider the drivers depend
//!   on; loads the refresh token from the store, caches the access token until just before
//!   expiry, refreshes transparently.
//! - [`GoogleApiClient`] — the authenticated API client: injects the bearer and **refreshes on
//!   a 401, retrying once** — the loop t20/t21/t41 reuse rather than re-implement.
//! - [`AccessToken`] / [`GoogleAccount`] — owned token/account DTOs whose values are
//!   [`qfs_secrets::Secret`] (redacting `Debug`/`Display`, no `Clone`/`Serialize`, zeroized on
//!   drop) — never logged, never serialized in the clear.
//! - [`AuthError`] — the structured, secret-free error taxonomy.
//!
//! ## Multi-account model
//! Every Google account is keyed `(google, <email>)` in the t27 store (one consent serves all
//! Google drivers). [`authorize`] resolves the email from the userinfo profile; a
//! [`StoredTokenSource`] is built **per email**, so two distinct accounts resolve to two
//! independent token sources and two independent access-token caches. The account *selection*
//! (which email to run as) reuses the t27 [`qfs_secrets::resolve`] ladder upstream — this crate
//! takes the resolved email.
//!
//! ## Purity / boundary discipline (RFD §3/§6/§9)
//! This crate performs network I/O for auth but exposes **only** a [`TokenSource`] + an
//! authenticated client — it constructs no `Plan` and no driver effect, so the effect-as-data
//! invariant of the driver layer is intact. `reqwest`/`url`/vendor types never cross a public
//! signature: network rides the thin, synchronous, **runtime-free** [`HttpExchange`] seam (a
//! mirror of the t18 `HttpClient` shape), and only owned DTOs + [`qfs_secrets::Secret`] cross
//! this crate's boundary. The seam is kept *local* (not a `qfs-driver-http` dependency) on
//! purpose: `qfs-driver-http` depends on `qfs-runtime`, and the workspace confinement invariant
//! requires every runtime consumer to be a leaf — so `qfs-google-auth` depends on **only**
//! `qfs-secrets` among workspace crates (which itself reaches only `qfs-types`), staying off the
//! runtime entirely. The consuming drivers (already runtime leaves holding an
//! `Arc<dyn qfs_driver_http::HttpClient>`) supply a trivial [`HttpExchange`] adapter over it, so
//! `reqwest` stays confined to `qfs-driver-http` exactly as before. The token machinery is
//! **synchronous** (matching that seam and every qfs driver's synchronous applier leg); no async
//! runtime is pulled into this crate or the spine.
//!
//! ## Secret safety (RFD §10) — the headline invariant
//! The `client_secret`, the authorization `code`, the access token, and the refresh token are
//! all [`qfs_secrets::Secret`] or move straight onto the wire form body. None enters a struct
//! field that is logged, an error `Display`/`Debug`, or a serialized record. The `Debug` of
//! every type here that holds key material is manual + redacting; the structured auth logs emit
//! the account email (a low-sensitivity id) and the flow lifecycle only. A redaction test
//! asserts a planted secret never surfaces on any text surface.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod client;
mod error;
mod http;
mod oauth;
mod source;
mod token;

#[cfg(not(target_arch = "wasm32"))]
mod authorize;

pub use client::GoogleApiClient;
pub use error::AuthError;
pub use http::{
    is_sensitive_header, HttpExchange, HttpMethod, HttpRequest, HttpResponse, MockExchange,
    TransportError, SENSITIVE_HEADERS,
};
pub use oauth::{
    OAuthClient, AUTH_ENDPOINT, LOOPBACK_REDIRECT_HOST, TOKEN_ENDPOINT, USERINFO_ENDPOINT,
};
pub use source::{
    decode_account_email, refresh_token_key, BorrowedToken, StoredTokenSource, TokenSource,
    GOOGLE_DRIVER_ID,
};
pub use token::{AccessToken, Clock, GoogleAccount, ManualClock, SystemClock, DEFAULT_EXPIRY_SKEW};

#[cfg(not(target_arch = "wasm32"))]
pub use authorize::{authorize, ConsentOpener};

#[cfg(test)]
mod tests;
