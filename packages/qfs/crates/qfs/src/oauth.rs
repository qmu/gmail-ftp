//! The `qfs serve` **OAuth-AS composition root** (t48 discovery + t49 live flow): the binary loads
//! the authorization server's ES256 signing key over the System DB, serves the three public discovery
//! routes (t48), and — as of t49 — serves the LIVE authorization-code + PKCE flow: dynamic client
//! registration (`POST /register`), the authorization endpoint (`GET`/`POST /authorize`, with a
//! minimal sign-in + consent screen over the t45 identity store + t46 sessions), and the token
//! endpoint (`POST /token`, code→signed-access-token). All routes are composed into the qfs-http
//! listener via the SAME `Fallback` seam t47 uses for `POST /mcp`.
//!
//! ## What is live (honesty)
//! The AS ISSUES tokens via the auth-code + PKCE (S256) grant: a human authenticates at `/authorize`
//! (t45 password verify → t46 session), consents, receives a short-lived single-use authorization
//! code, and the client exchanges it at `/token` for a signed ES256 access token (+ a refresh-token
//! handle stored hashed). As of **t50** the token is also CONSUMED: `/token` additionally serves the
//! `grant_type=refresh_token` grant (single-use rotation — mint a fresh access token + a new refresh
//! handle, burn the old; a replay of a rotated handle is `invalid_grant`), and the binary lifts the
//! [`OauthRoutes::mcp_verification`] material (JWKS + issuer/audience + PRM URL) to bearer-gate the
//! `POST /mcp` endpoint (a missing/invalid/expired token → `401` + `WWW-Authenticate`). So the full
//! discover → register → auth-code+PKCE → bearer-gated-MCP handshake is real end-to-end.
//!
//! ## Security (RFD §10)
//! - **PKCE S256 is mandatory**; `plain` is refused; the verifier is checked constant-time at `/token`.
//! - **Authorization codes** are single-use, short-TTL, bound to the exact `client_id` +
//!   `redirect_uri` + PKCE challenge + authenticated `user_id`, stored HASHED, and burned on first
//!   exchange (a replay finds nothing).
//! - **`redirect_uri`** is EXACT-matched against the client's registered allowlist (no
//!   prefix/substring matching) — and a request to an unregistered redirect is NEVER redirected.
//! - The access token is a JWS signed with the t48 ES256 key (`iss`/`aud`/`sub`/`scope`/`exp`).
//! - The PRIVATE signing key is envelope-decrypted into a `Secret` only inside this module; NO code,
//!   token, secret, verifier, or key is ever logged. `Set-Cookie` rides the redacted header set.
//! - The consent POST carries a synchronizer CSRF token (the session hash, which the client never
//!   sees) and the session is ROTATED on consent (fixation defense). **Consent is minimal** this
//!   milestone (a single approve/deny screen, no scope-by-scope grant) — flagged for the reviewer.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use qfs_http::{HttpRequest, HttpResponse, Method};
use qfs_identity::{IdentityStore, PROVIDER_LOCAL};
use qfs_oauth::{
    access_token_claims, error_json, redirect_uri_is_registered, sign_jws,
    validate_authorize_request, validate_refresh_request, validate_registration,
    validate_token_request, verify_jws, verify_pkce_s256, AuthorizationServerMetadata,
    AuthorizeRequest, ClientRegistrationRequest, ClientRegistrationResponse, Jwk, Jwks,
    OAuthFlowError, ProtectedResourceMetadata, RefreshTokenRequest, SigningKey, TokenRequest,
    TokenResponse, ALG_ES256, AUTHORIZE_PATH, GRANT_AUTHORIZATION_CODE, GRANT_REFRESH_TOKEN,
    PKCE_METHOD_S256, REGISTER_PATH, TOKEN_PATH,
};
use qfs_secrets::Secret;
use qfs_session::{
    authenticate, format_set_cookie, parse_cookie_header, token_hash, SessionStore, UserId,
    DEFAULT_SESSION_TTL_SECS,
};
use qfs_store::{OauthKeyStore, SqliteIdentityStore, SqliteOauthFlowStore, SqliteSessionStore};
use rand::RngCore;

/// The P-256 secret-scalar width: 32 bytes of OS entropy per generated key.
const ENTROPY_BYTES: usize = 32;

/// The RFC 9728 Protected Resource Metadata well-known path.
const PRM_PATH: &str = "/.well-known/oauth-protected-resource";
/// The RFC 8414 Authorization Server Metadata well-known path.
const ASM_PATH: &str = "/.well-known/oauth-authorization-server";
/// The JWKS document path.
const JWKS_PATH: &str = "/jwks.json";

/// Authorization-code TTL: short (60s) — a code lives only long enough for the client to exchange it.
const CODE_TTL_SECS: i64 = 60;
/// Access-token lifetime (10 minutes) — conservative; the refresh token (t50) re-mints it.
const ACCESS_TTL_SECS: u64 = 600;
/// Refresh-token-handle lifetime (30 days). Issued here; ENFORCED/ROTATED in t50.
const REFRESH_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// Seconds since the Unix epoch (for the token `iat`/`exp` window + the DCR `client_id_issued_at`).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The live OAuth-AS routes: the three pre-rendered public discovery bodies (t48) plus the t49 flow
/// state (`None` only if the flow stores could not be opened — discovery then serves best-effort).
/// Held behind an `Arc` and shared across the listener's connections.
pub struct OauthRoutes {
    prm: Vec<u8>,
    asm: Vec<u8>,
    jwks: Vec<u8>,
    flow: Option<FlowState>,
    /// The material the binary's MCP middleware needs to validate bearer access tokens (t50): the
    /// published JWKS + the expected issuer/audience + the PRM URL the `401` challenge points at.
    /// Present whenever the signing key + discovery documents are ready (independent of the live flow
    /// stores) — so a server that can VERIFY tokens can gate `/mcp` even if its flow stores are down.
    verification: McpVerification,
}

/// The bearer-validation material the binary lifts out of the booted AS to build the MCP authorizer
/// (t50). Carries only PUBLIC key material (the JWKS) + the issuer/audience/PRM strings — no signing
/// key, no secret.
#[derive(Clone)]
pub struct McpVerification {
    /// The published JWKS the access-token signature is verified against.
    pub jwks: Jwks,
    /// The expected access-token issuer (the AS origin; proxy-aware via `QFS_OAUTH_ISSUER`).
    pub issuer: String,
    /// The expected access-token audience (the MCP resource URL `issuer + /mcp`).
    pub audience: String,
    /// The Protected-Resource-Metadata URL the `WWW-Authenticate` challenge points a client at.
    pub prm_url: String,
}

/// The live authorization-code + PKCE flow state: the retained ES256 signing key (to mint tokens) +
/// the three System-DB-backed stores (clients/codes, sessions, identity) + the issuer/resource the
/// token binds to. Never `Debug` (it holds the signing key).
struct FlowState {
    /// The AS issuer origin (token `iss`).
    issuer: String,
    /// The MCP resource the token is good for (token `aud`).
    resource: String,
    /// The active ES256 signing key (retained from boot to mint access tokens).
    signing_key: SigningKey,
    /// Registered clients + short-lived authorization codes + refresh handles (t49).
    clients: SqliteOauthFlowStore,
    /// Server-side sessions (t46) — the authenticated human carried through the flow.
    sessions: SqliteSessionStore,
    /// The identity store (t45) — the local password verify at sign-in.
    identity: SqliteIdentityStore,
    /// Whether to mark the session cookie `Secure` (a trusted-HTTPS issuer).
    secure_cookies: bool,
}

impl OauthRoutes {
    /// The bearer-validation material the serve composition root uses to build the MCP authorizer
    /// (t50) — the published JWKS + the expected issuer/audience + the PRM URL for the `401`
    /// challenge. Always available once the AS booted (the signing key + discovery rendered).
    #[must_use]
    pub fn mcp_verification(&self) -> &McpVerification {
        &self.verification
    }

    /// Serve an OAuth route if `req` matches: the three GET discovery routes (always), and — when the
    /// flow stores are live — `POST /register`, `GET`/`POST /authorize`, and `POST /token`. Returns
    /// `None` (falls through to the next handler / 404) for anything else.
    #[must_use]
    pub fn handle(&self, req: &HttpRequest) -> Option<HttpResponse> {
        // Read-only discovery (GET): public, cacheable, no creds.
        if req.method == Method::Get {
            match req.path.as_str() {
                PRM_PATH => return Some(json_response(200, self.prm.clone())),
                ASM_PATH => return Some(json_response(200, self.asm.clone())),
                JWKS_PATH => return Some(json_response(200, self.jwks.clone())),
                _ => {}
            }
        }
        let flow = self.flow.as_ref()?;
        if req.path == REGISTER_PATH && req.method == Method::Post {
            return Some(flow.handle_register(req));
        }
        if req.path == AUTHORIZE_PATH {
            return match req.method {
                Method::Get => Some(flow.handle_authorize_get(req)),
                Method::Post => Some(flow.handle_authorize_post(req)),
                _ => None,
            };
        }
        if req.path == TOKEN_PATH && req.method == Method::Post {
            return Some(flow.handle_token(req));
        }
        None
    }
}

impl FlowState {
    /// `POST /register` — dynamic client registration (RFC 7591). Validate the request, mint a public
    /// `client_id` (no secret — PKCE is the proof), persist it, and return the registration response.
    fn handle_register(&self, req: &HttpRequest) -> HttpResponse {
        let dto: ClientRegistrationRequest = match serde_json::from_slice(&req.body) {
            Ok(d) => d,
            Err(_) => {
                return registration_error(
                    400,
                    "invalid_client_metadata",
                    "the request body is not valid JSON",
                )
            }
        };
        if let Err(e) = validate_registration(&dto) {
            return registration_error(400, e.code(), &e.to_string());
        }
        let client_id = random_opaque();
        if self
            .clients
            .register_client(
                &client_id,
                &dto.redirect_uris,
                dto.client_name.as_deref(),
                None,
            )
            .is_err()
        {
            return registration_error(500, "server_error", "the client could not be persisted");
        }
        let resp = ClientRegistrationResponse::public_client(
            client_id,
            dto.redirect_uris,
            dto.client_name,
            now_unix(),
        );
        match serde_json::to_vec(&resp) {
            Ok(body) => json_response(201, body),
            Err(_) => registration_error(500, "server_error", "the response could not be rendered"),
        }
    }

    /// `GET /authorize` — validate the request, then render a minimal sign-in (no session) or consent
    /// (live session) screen. NO state changes here (GET is safe): the human's approval is the POST.
    fn handle_authorize_get(&self, req: &HttpRequest) -> HttpResponse {
        let areq = authorize_request_from(&req.query);
        // 1. Resolve + EXACT-match the client/redirect BEFORE anything is redirected.
        let registered = match self.lookup_redirect_allowlist(&areq) {
            Ok(uris) => uris,
            Err(resp) => return resp,
        };
        let _ = registered;
        // 2. Validate the protocol params (PKCE S256 mandatory, state required, response_type=code).
        if let Err(e) = validate_authorize_request(&areq) {
            return redirect_error(&areq.redirect_uri, e.code(), &areq.state);
        }
        // 3. Require an authenticated session; if absent, render the sign-in form.
        let cookie = req.headers.get("cookie").map(String::as_str);
        match authenticate(cookie, &self.sessions) {
            Ok(Some(_uid)) => {
                let csrf = current_session_hash(cookie).unwrap_or_default();
                html_response(200, consent_page(&areq, &csrf))
            }
            _ => html_response(200, signin_page(&areq, None)),
        }
    }

    /// `POST /authorize` — the human's sign-in + consent submission. Re-validate everything,
    /// authenticate (existing session or local password), enforce CSRF on the consent path, and on
    /// approval ROTATE the session, mint a single-use authorization code bound to the request, and
    /// redirect back to the registered `redirect_uri` with `code` + `state`.
    fn handle_authorize_post(&self, req: &HttpRequest) -> HttpResponse {
        let form = parse_form(&req.body);
        let areq = authorize_request_from(&form);
        // 1. Re-resolve + EXACT-match the client/redirect.
        if let Err(resp) = self.lookup_redirect_allowlist(&areq) {
            return resp;
        }
        // 2. Re-validate the protocol params.
        if let Err(e) = validate_authorize_request(&areq) {
            return redirect_error(&areq.redirect_uri, e.code(), &areq.state);
        }
        let decision = form.get("decision").map(String::as_str).unwrap_or("");
        if decision == "deny" {
            return redirect_error(&areq.redirect_uri, "access_denied", &areq.state);
        }
        // 3. Authenticate: an existing session (CSRF-checked + rotated) or a fresh password sign-in.
        let cookie = req.headers.get("cookie").map(String::as_str);
        let (user_id, set_cookie) = match authenticate(cookie, &self.sessions) {
            Ok(Some(uid)) => {
                let current = current_session_hash(cookie).unwrap_or_default();
                let submitted = form.get("csrf").map(String::as_str).unwrap_or("");
                if current.is_empty() || submitted != current {
                    return html_response(
                        403,
                        error_page(
                            "The consent request could not be verified (CSRF). Please retry.",
                        ),
                    );
                }
                // Rotate the session on consent (fixation defense); keep the old one if rotate fails.
                (uid, self.rotate_session(&current))
            }
            _ => match self.signin_from_form(&form) {
                Ok(pair) => pair,
                Err(resp) => return resp,
            },
        };
        if decision != "approve" {
            return html_response(400, error_page("Missing consent decision."));
        }
        // 4. Mint the single-use authorization code bound to (client, redirect, PKCE, user, scope).
        let (code, code_hash) = random_opaque_with_hash();
        if self
            .clients
            .insert_code(
                &code_hash,
                &areq.client_id,
                user_id.0,
                &areq.redirect_uri,
                &areq.code_challenge,
                &areq.code_challenge_method,
                &areq.scope,
                CODE_TTL_SECS,
            )
            .is_err()
        {
            return redirect_error(&areq.redirect_uri, "server_error", &areq.state);
        }
        // 5. Redirect back with code + state (+ Set-Cookie if a session was minted/rotated).
        let location = build_redirect(&areq.redirect_uri, &code, &areq.state);
        let mut resp = HttpResponse::new(302, "text/plain; charset=utf-8", Vec::new())
            .with_header("Location", location)
            .with_header("Cache-Control", "no-store");
        if let Some(sc) = set_cookie {
            resp = resp.with_header("Set-Cookie", sc);
        }
        resp
    }

    /// `POST /token` — the token endpoint, dispatched on `grant_type`: the auth-code exchange
    /// ([`handle_authorization_code`](Self::handle_authorization_code)) OR the refresh-token grant
    /// ([`handle_refresh`](Self::handle_refresh), t50). An unknown grant is `unsupported_grant_type`.
    fn handle_token(&self, req: &HttpRequest) -> HttpResponse {
        let form = parse_form(&req.body);
        match form.get("grant_type").map(String::as_str).unwrap_or("") {
            GRANT_AUTHORIZATION_CODE => self.handle_authorization_code(&form),
            GRANT_REFRESH_TOKEN => self.handle_refresh(&form),
            _ => token_error(400, OAuthFlowError::UnsupportedGrantType),
        }
    }

    /// The authorization-code exchange: verify a single-use code (against its bound client + redirect
    /// + PKCE verifier) and mint a signed ES256 access token + a (first-issue) hashed refresh handle.
    fn handle_authorization_code(&self, form: &BTreeMap<String, String>) -> HttpResponse {
        let treq = token_request_from(form);
        if let Err(e) = validate_token_request(&treq) {
            return token_error(400, e);
        }
        // Redeem (and BURN) the code by its hash — single-use; a replay finds nothing.
        let redeemed = match self.clients.take_code(&token_hash(&treq.code)) {
            Ok(Some(r)) => r,
            Ok(None) => return token_error(400, OAuthFlowError::InvalidGrant),
            Err(_) => return token_error(500, OAuthFlowError::ServerError),
        };
        // Re-bind: the code must have been issued to THIS client + redirect.
        if redeemed.client_id != treq.client_id || redeemed.redirect_uri != treq.redirect_uri {
            return token_error(400, OAuthFlowError::InvalidGrant);
        }
        // Verify the PKCE verifier against the code's stored S256 challenge (constant-time).
        if redeemed.pkce_method != PKCE_METHOD_S256
            || !verify_pkce_s256(&treq.code_verifier, &redeemed.pkce_challenge)
        {
            return token_error(400, OAuthFlowError::InvalidGrant);
        }
        // Mint the signed access token.
        let now = now_unix();
        let claims = access_token_claims(
            &self.issuer,
            &self.resource,
            redeemed.user_id,
            &redeemed.scope,
            &redeemed.client_id,
            now,
            ACCESS_TTL_SECS,
        );
        let access = match sign_jws(&claims, &self.signing_key) {
            Ok(t) => t,
            Err(_) => return token_error(500, OAuthFlowError::ServerError),
        };
        // Issue a refresh-token handle, stored ONLY as its hash (enforced/refreshed in t50).
        let (refresh, refresh_hash) = random_opaque_with_hash();
        let _ = self.clients.insert_refresh(
            &refresh_hash,
            redeemed.user_id,
            &redeemed.client_id,
            &redeemed.scope,
            REFRESH_TTL_SECS,
        );
        let resp = TokenResponse::bearer(access, ACCESS_TTL_SECS, Some(refresh), redeemed.scope);
        match serde_json::to_vec(&resp) {
            Ok(body) => json_response(200, body).with_header("Cache-Control", "no-store"),
            Err(_) => token_error(500, OAuthFlowError::ServerError),
        }
    }

    /// The **refresh-token grant** (t50, OAuth 2.1 §4.3 with refresh-token ROTATION): redeem the
    /// presented refresh handle by its HASH, single-use BURN it (a replay of a rotated/stale handle
    /// finds nothing → `invalid_grant`), mint a fresh access token, and issue a NEW refresh handle
    /// (recording `rotated_from` for lineage). Only stored hashes are compared — the plaintext handle
    /// exists only in the response that delivers it. The handle is never logged.
    fn handle_refresh(&self, form: &BTreeMap<String, String>) -> HttpResponse {
        let rreq = refresh_request_from(form);
        if let Err(e) = validate_refresh_request(&rreq) {
            return token_error(400, e);
        }
        // Hashed lookup + single-use burn (rotation). An unknown/expired/already-rotated handle is an
        // `invalid_grant` — the replay of a leaked-but-rotated handle is rejected here.
        let old_hash = token_hash(&rreq.refresh_token);
        let redeemed = match self.clients.take_refresh(&old_hash) {
            Ok(Some(r)) => r,
            Ok(None) => return token_error(400, OAuthFlowError::InvalidGrant),
            Err(_) => return token_error(500, OAuthFlowError::ServerError),
        };
        // Re-bind: the handle must have been issued to THIS client (defense in depth).
        if redeemed.client_id != rreq.client_id {
            return token_error(400, OAuthFlowError::InvalidGrant);
        }
        // Mint the fresh access token over the handle's bound user + scope.
        let now = now_unix();
        let claims = access_token_claims(
            &self.issuer,
            &self.resource,
            redeemed.user_id,
            &redeemed.scope,
            &redeemed.client_id,
            now,
            ACCESS_TTL_SECS,
        );
        let access = match sign_jws(&claims, &self.signing_key) {
            Ok(t) => t,
            Err(_) => return token_error(500, OAuthFlowError::ServerError),
        };
        // Rotate: issue a NEW refresh handle (stored hashed) recording the prior handle's hash.
        let (refresh, refresh_hash) = random_opaque_with_hash();
        let _ = self.clients.insert_refresh_rotated(
            &refresh_hash,
            redeemed.user_id,
            &redeemed.client_id,
            &redeemed.scope,
            REFRESH_TTL_SECS,
            &old_hash,
        );
        let resp = TokenResponse::bearer(access, ACCESS_TTL_SECS, Some(refresh), redeemed.scope);
        match serde_json::to_vec(&resp) {
            Ok(body) => json_response(200, body).with_header("Cache-Control", "no-store"),
            Err(_) => token_error(500, OAuthFlowError::ServerError),
        }
    }

    /// Look the client up and EXACT-match `req.redirect_uri` against its registered allowlist. On an
    /// unknown client or an unregistered redirect, return an Err carrying an HTML error PAGE (never a
    /// redirect — we must not bounce to an unvalidated URI). On success, the registered allowlist.
    fn lookup_redirect_allowlist(
        &self,
        req: &AuthorizeRequest,
    ) -> Result<Vec<String>, HttpResponse> {
        let client = match self.clients.find_client(&req.client_id) {
            Ok(Some(c)) => c,
            Ok(None) => return Err(html_response(400, error_page("Unknown client_id."))),
            Err(_) => {
                return Err(html_response(
                    500,
                    error_page("The authorization server is unavailable."),
                ))
            }
        };
        if !redirect_uri_is_registered(&req.redirect_uri, &client.redirect_uris) {
            return Err(html_response(
                400,
                error_page("The redirect_uri does not match a registered URI for this client."),
            ));
        }
        Ok(client.redirect_uris)
    }

    /// Authenticate a fresh sign-in from the consent form's `email`/`password`, mint a session, and
    /// return `(user_id, Some(set_cookie))`. On a bad/empty credential, re-render the sign-in form.
    fn signin_from_form(
        &self,
        form: &BTreeMap<String, String>,
    ) -> Result<(UserId, Option<String>), HttpResponse> {
        let areq = authorize_request_from(form);
        let email = form.get("email").map(String::as_str).unwrap_or("");
        let password = form.get("password").map(String::as_str).unwrap_or("");
        if email.is_empty() || password.is_empty() {
            return Err(html_response(
                401,
                signin_page(&areq, Some("Enter your email and password.")),
            ));
        }
        let candidate = Secret::from(password);
        let ok = self
            .identity
            .verify_password(PROVIDER_LOCAL, email, &candidate)
            .unwrap_or(false);
        if !ok {
            return Err(html_response(
                401,
                signin_page(&areq, Some("Invalid email or password.")),
            ));
        }
        let user = match self.identity.find_user_by_email(email) {
            Ok(Some(u)) => u,
            _ => {
                return Err(html_response(
                    401,
                    signin_page(&areq, Some("Invalid email or password.")),
                ))
            }
        };
        match crate::session::issue_session(&self.sessions, user.id, self.secure_cookies) {
            Ok((_session, set_cookie)) => Ok((user.id, Some(set_cookie))),
            Err(_) => Err(html_response(
                500,
                error_page("Could not establish a session."),
            )),
        }
    }

    /// Rotate the live session (fixation defense) and return the new `Set-Cookie`, or `None` (keep the
    /// existing session) if the rotate fails.
    fn rotate_session(&self, old_hash: &str) -> Option<String> {
        let new = crate::session::generate_token();
        match self
            .sessions
            .rotate(old_hash, &new.hash(), DEFAULT_SESSION_TTL_SECS)
        {
            Ok(_) => Some(format_set_cookie(
                &new,
                DEFAULT_SESSION_TTL_SECS,
                self.secure_cookies,
            )),
            Err(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------------------------------

/// Boot the OAuth AS: open the System DB, unlock (or initialize) the OAuth key envelope with
/// `QFS_PASSPHRASE`, load-or-generate the active ES256 key, self-verify it, pre-render the discovery
/// documents, and — if the flow stores open — wire the live auth-code + PKCE flow. Returns `None`
/// (best-effort, logged) when the System DB or passphrase is unavailable.
#[must_use]
pub fn boot_oauth(addr: SocketAddr) -> Option<Arc<OauthRoutes>> {
    let store = match open_key_store() {
        Ok(Some(store)) => store,
        Ok(None) => {
            tracing::info!(
                target: "qfs::oauth",
                "oauth AS not served (no System DB or QFS_PASSPHRASE); set QFS_PASSPHRASE to enable"
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(target: "qfs::oauth", error = %e, "oauth AS key store unavailable; not serving discovery routes");
            return None;
        }
    };

    let issuer = issuer_from_addr(addr);
    match build_routes_from_store(&store, &issuer) {
        Ok(routes) => Some(Arc::new(routes)),
        Err(e) => {
            tracing::warn!(target: "qfs::oauth", error = %e, "oauth AS could not initialize signing key; not serving discovery routes");
            None
        }
    }
}

/// Open the System DB at the default path and build the [`OauthKeyStore`] over its owned connection,
/// unlocking with `QFS_PASSPHRASE`. `Ok(None)` when the config home or the passphrase is absent.
fn open_key_store() -> Result<Option<OauthKeyStore>, String> {
    let Some(sys) =
        crate::store::open_system_db().map_err(|e| format!("opening the system database: {e}"))?
    else {
        return Ok(None);
    };
    let pass = match std::env::var("QFS_PASSPHRASE") {
        Ok(p) if !p.is_empty() => p,
        _ => return Ok(None),
    };
    let store = OauthKeyStore::from_db(sys.into_db(), &Secret::from(pass))
        .map_err(|e| format!("unlocking the oauth key store: {e}"))?;
    Ok(Some(store))
}

/// Load the active key (or generate + persist one on first boot), self-verify it, render the
/// discovery documents, and wire the live flow state (best-effort: a flow-store failure leaves the
/// discovery routes serving and logs).
fn build_routes_from_store(store: &OauthKeyStore, issuer: &str) -> Result<OauthRoutes, String> {
    let key = ensure_active_signing_key(store)?;
    self_verify(&key)?;
    tracing::info!(target: "qfs::oauth", kid = %key.kid(), %issuer, "oauth AS signing key ready (ES256); serving discovery + auth-code/PKCE flow (tokens issued; MCP not yet gated — t50)");

    let jwks = published_jwks(store)?;
    let discovery = build_discovery(issuer, &jwks)?;

    // The bearer-validation material (t50): the resource server can VERIFY tokens as soon as the
    // JWKS + issuer are known, independently of whether the live flow stores opened.
    let verification = McpVerification {
        jwks,
        issuer: issuer.to_string(),
        audience: format!("{issuer}{}", qfs_mcp::MCP_PATH),
        prm_url: format!("{issuer}{PRM_PATH}"),
    };

    let flow = match build_flow(issuer, key) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!(target: "qfs::oauth", error = %e, "oauth AS flow stores unavailable; serving discovery only");
            None
        }
    };
    Ok(OauthRoutes {
        prm: discovery.prm,
        asm: discovery.asm,
        jwks: discovery.jwks,
        flow,
        verification,
    })
}

/// Open the three System-DB-backed flow stores (clients/codes, sessions, identity) over their own
/// connections to the same System DB, and assemble the live [`FlowState`] retaining the signing key.
fn build_flow(issuer: &str, key: SigningKey) -> Result<FlowState, String> {
    let sys = crate::store::open_system_db()
        .map_err(|e| format!("opening the system database for the oauth flow: {e}"))?
        .ok_or("cannot determine the system database path (set HOME or XDG_CONFIG_HOME)")?;
    let clients = SqliteOauthFlowStore::from_db(sys.into_db());
    let sessions = crate::session::open_session_store()?;
    let identity = crate::identity::open_identity_store()?;
    let resource = format!("{issuer}{}", qfs_mcp::MCP_PATH);
    let secure_cookies = issuer.starts_with("https://");
    Ok(FlowState {
        issuer: issuer.to_string(),
        resource,
        signing_key: key,
        clients,
        sessions,
        identity,
        secure_cookies,
    })
}

/// Reload the active signing key, or generate + persist a fresh one if none exists (so a SECOND boot
/// reuses the first boot's key). Generation draws 32 bytes of OS entropy; a rare out-of-range scalar
/// is retried.
fn ensure_active_signing_key(store: &OauthKeyStore) -> Result<SigningKey, String> {
    if let Some(stored) = store
        .active_key()
        .map_err(|e| format!("reading the active oauth key: {e}"))?
    {
        return SigningKey::from_secret_scalar(&stored.private_scalar)
            .map_err(|e| format!("reconstructing the active oauth key: {e}"));
    }

    for _ in 0..8 {
        let mut entropy = [0u8; ENTROPY_BYTES];
        rand::rng().fill_bytes(&mut entropy);
        let Ok(key) = SigningKey::generate(&entropy) else {
            continue;
        };
        let public_jwk = serde_json::to_string(&key.public_jwk())
            .map_err(|e| format!("rendering the oauth public JWK: {e}"))?;
        store
            .insert_active_key(key.kid(), ALG_ES256, &public_jwk, &key.secret_scalar())
            .map_err(|e| format!("persisting the new oauth key: {e}"))?;
        return Ok(key);
    }
    Err("could not generate a valid ES256 signing key from OS entropy".to_string())
}

/// Prove the freshly loaded signing key is operational end-to-end: sign a probe and verify it against
/// the key's own public JWK. The probe carries no secret and is discarded (never logged).
fn self_verify(key: &SigningKey) -> Result<(), String> {
    let probe = serde_json::json!({ "kid": key.kid(), "probe": true });
    let token =
        sign_jws(&probe, key).map_err(|e| format!("oauth key self-test sign failed: {e}"))?;
    let jwks = Jwks::new(vec![key.public_jwk()]);
    let claims =
        verify_jws(&token, &jwks).map_err(|e| format!("oauth key self-test verify failed: {e}"))?;
    if claims != probe {
        return Err("oauth key self-test round-trip mismatch".to_string());
    }
    Ok(())
}

/// Parse every published public-JWK JSON string into a [`Jwk`] and collect them into the JWKS.
fn published_jwks(store: &OauthKeyStore) -> Result<Jwks, String> {
    let raw = store
        .published_public_jwks()
        .map_err(|e| format!("reading the published JWKS: {e}"))?;
    let mut keys = Vec::with_capacity(raw.len());
    for s in &raw {
        let jwk: Jwk =
            serde_json::from_str(s).map_err(|e| format!("parsing a stored public JWK: {e}"))?;
        keys.push(jwk);
    }
    Ok(Jwks::new(keys))
}

/// The three pre-rendered public discovery JSON bodies (PRM, AS metadata, JWKS).
struct DiscoveryBodies {
    prm: Vec<u8>,
    asm: Vec<u8>,
    jwks: Vec<u8>,
}

/// Render the three discovery documents for `issuer` + `jwks` into their JSON bodies (PRM, AS
/// metadata advertising the now-live endpoints, JWKS).
fn build_discovery(issuer: &str, jwks: &Jwks) -> Result<DiscoveryBodies, String> {
    let resource = format!("{issuer}{}", qfs_mcp::MCP_PATH);
    let jwks_uri = format!("{issuer}{JWKS_PATH}");
    let prm = ProtectedResourceMetadata::new(resource, issuer);
    let asm = AuthorizationServerMetadata::new(issuer, jwks_uri);
    Ok(DiscoveryBodies {
        prm: serde_json::to_vec(&prm).map_err(|e| format!("rendering PRM: {e}"))?,
        asm: serde_json::to_vec(&asm).map_err(|e| format!("rendering AS metadata: {e}"))?,
        jwks: serde_json::to_vec(jwks).map_err(|e| format!("rendering JWKS: {e}"))?,
    })
}

/// Derive the AS issuer (origin URL) from the listener's bind address. `QFS_OAUTH_ISSUER` overrides
/// it for the trusted-reverse-proxy case (decision F). A loopback bind advertises
/// `http://localhost:<port>`; otherwise `http://<addr>`.
fn issuer_from_addr(addr: SocketAddr) -> String {
    if let Ok(explicit) = std::env::var("QFS_OAUTH_ISSUER") {
        if !explicit.is_empty() {
            return explicit.trim_end_matches('/').to_string();
        }
    }
    if addr.ip().is_loopback() {
        format!("http://localhost:{}", addr.port())
    } else {
        format!("http://{addr}")
    }
}

// ---------------------------------------------------------------------------------------------------
// Pure HTTP / parsing / rendering helpers
// ---------------------------------------------------------------------------------------------------

/// A `200`/`201` JSON response with `Cache-Control: no-store` left to the caller where it matters.
fn json_response(status: u16, body: Vec<u8>) -> HttpResponse {
    HttpResponse::new(status, "application/json", body)
}

/// An HTML response (the sign-in / consent / error screens).
fn html_response(status: u16, body: String) -> HttpResponse {
    HttpResponse::new(status, "text/html; charset=utf-8", body.into_bytes())
        .with_header("Cache-Control", "no-store")
}

/// A registration-error JSON body (RFC 7591 §3.2.2: `{"error":..,"error_description":..}`).
fn registration_error(status: u16, code: &str, description: &str) -> HttpResponse {
    let body = serde_json::json!({ "error": code, "error_description": description });
    json_response(status, serde_json::to_vec(&body).unwrap_or_default())
}

/// A token-endpoint OAuth-error JSON body (RFC 6749 §5.2) with `Cache-Control: no-store`.
fn token_error(status: u16, err: OAuthFlowError) -> HttpResponse {
    json_response(
        status,
        serde_json::to_vec(&error_json(err)).unwrap_or_default(),
    )
    .with_header("Cache-Control", "no-store")
}

/// A 302 redirect back to `redirect_uri` carrying an OAuth `error` + the echoed `state` (RFC 6749
/// §4.1.2.1). Used only AFTER `redirect_uri` is validated against the registered allowlist.
fn redirect_error(redirect_uri: &str, code: &str, state: &str) -> HttpResponse {
    let sep = if redirect_uri.contains('?') { '&' } else { '?' };
    let mut location = format!("{redirect_uri}{sep}error={}", urlencode(code));
    if !state.is_empty() {
        location.push_str(&format!("&state={}", urlencode(state)));
    }
    HttpResponse::new(302, "text/plain; charset=utf-8", Vec::new())
        .with_header("Location", location)
        .with_header("Cache-Control", "no-store")
}

/// Build the success redirect `redirect_uri?code=<code>&state=<state>` (exact-validated URI).
fn build_redirect(redirect_uri: &str, code: &str, state: &str) -> String {
    let sep = if redirect_uri.contains('?') { '&' } else { '?' };
    let mut out = format!("{redirect_uri}{sep}code={}", urlencode(code));
    if !state.is_empty() {
        out.push_str(&format!("&state={}", urlencode(state)));
    }
    out
}

/// Extract an [`AuthorizeRequest`] from a parameter map (the GET query or the POST form), defaulting
/// each missing parameter to the empty string (validation then rejects the empties).
fn authorize_request_from(map: &BTreeMap<String, String>) -> AuthorizeRequest {
    let get = |k: &str| map.get(k).cloned().unwrap_or_default();
    AuthorizeRequest {
        response_type: get("response_type"),
        client_id: get("client_id"),
        redirect_uri: get("redirect_uri"),
        scope: get("scope"),
        state: get("state"),
        code_challenge: get("code_challenge"),
        code_challenge_method: get("code_challenge_method"),
    }
}

/// Extract a [`TokenRequest`] from the POST form map.
fn token_request_from(map: &BTreeMap<String, String>) -> TokenRequest {
    let get = |k: &str| map.get(k).cloned().unwrap_or_default();
    TokenRequest {
        grant_type: get("grant_type"),
        code: get("code"),
        redirect_uri: get("redirect_uri"),
        client_id: get("client_id"),
        code_verifier: get("code_verifier"),
    }
}

/// Extract a [`RefreshTokenRequest`] from the POST form map (the refresh-token grant, t50).
fn refresh_request_from(map: &BTreeMap<String, String>) -> RefreshTokenRequest {
    let get = |k: &str| map.get(k).cloned().unwrap_or_default();
    RefreshTokenRequest {
        grant_type: get("grant_type"),
        refresh_token: get("refresh_token"),
        client_id: get("client_id"),
    }
}

/// The live session hash for a request's `Cookie` header (the synchronizer CSRF token + the rotate
/// key), or `None` if no `qfs_session` cookie is present.
fn current_session_hash(cookie_header: Option<&str>) -> Option<String> {
    cookie_header
        .and_then(parse_cookie_header)
        .map(|tok| token_hash(&tok))
}

/// Mint an opaque high-entropy token string (64 lowercase-hex chars) — a `client_id`, an
/// authorization code, or a refresh handle. The binary owns the CSPRNG (the same entropy-injection
/// discipline the session token uses).
fn random_opaque() -> String {
    crate::session::generate_token()
        .reveal()
        .expose_str()
        .unwrap_or("")
        .to_string()
}

/// Mint an opaque token and its at-rest `sha256_hex` together — for codes/handles stored hashed.
fn random_opaque_with_hash() -> (String, String) {
    let token = crate::session::generate_token();
    let value = token.reveal().expose_str().unwrap_or("").to_string();
    (value, token.hash())
}

/// Parse an `application/x-www-form-urlencoded` body into a parameter map (last-wins), percent/plus
/// decoded. The untrusted-input boundary for the POST handlers.
fn parse_form(body: &[u8]) -> BTreeMap<String, String> {
    let s = String::from_utf8_lossy(body);
    let mut out = BTreeMap::new();
    for pair in s.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.insert(form_decode(k), form_decode(v));
    }
    out
}

/// Minimal `+`/`%XX` form decoding (the inverse of [`urlencode`] for the parameters we round-trip).
fn form_decode(s: &str) -> String {
    let spaced = s.replace('+', " ");
    let raw = spaced.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            if let Ok(byte) = u8::from_str_radix(&spaced[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a query/redirect value: keep the RFC 3986 unreserved set, escape everything else.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// HTML-escape an untrusted value before embedding it in a form/page (XSS defense for the reflected
/// authorize parameters).
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// The hidden authorize-parameter fields shared by the sign-in + consent forms (reflected, escaped).
fn hidden_authorize_fields(req: &AuthorizeRequest) -> String {
    let field = |name: &str, value: &str| {
        format!(
            "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
            name,
            html_escape(value)
        )
    };
    [
        field("response_type", &req.response_type),
        field("client_id", &req.client_id),
        field("redirect_uri", &req.redirect_uri),
        field("scope", &req.scope),
        field("state", &req.state),
        field("code_challenge", &req.code_challenge),
        field("code_challenge_method", &req.code_challenge_method),
    ]
    .join("\n")
}

/// A minimal, self-contained sign-in page (no external assets — the future-SPA constraint): the
/// human's email + password over the reflected authorize parameters, plus approve/deny.
fn signin_page(req: &AuthorizeRequest, error: Option<&str>) -> String {
    let err = error
        .map(|e| format!("<p style=\"color:#b00\">{}</p>", html_escape(e)))
        .unwrap_or_default();
    let scope = if req.scope.is_empty() {
        "(default access)"
    } else {
        &req.scope
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>qfs — sign in</title></head>\
         <body><h1>Sign in to qfs</h1>\
         <p>An application is requesting access: <code>{scope}</code>.</p>{err}\
         <form method=\"post\" action=\"{AUTHORIZE_PATH}\">\
         {hidden}\
         <p><label>Email <input type=\"email\" name=\"email\" autocomplete=\"username\"></label></p>\
         <p><label>Password <input type=\"password\" name=\"password\" autocomplete=\"current-password\"></label></p>\
         <p><button type=\"submit\" name=\"decision\" value=\"approve\">Sign in &amp; allow</button> \
         <button type=\"submit\" name=\"decision\" value=\"deny\">Deny</button></p>\
         </form></body></html>",
        scope = html_escape(scope),
        err = err,
        hidden = hidden_authorize_fields(req),
    )
}

/// A minimal consent page for an already-signed-in human: the reflected authorize parameters + the
/// CSRF synchronizer token + approve/deny.
fn consent_page(req: &AuthorizeRequest, csrf: &str) -> String {
    let scope = if req.scope.is_empty() {
        "(default access)"
    } else {
        &req.scope
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>qfs — authorize</title></head>\
         <body><h1>Authorize application</h1>\
         <p>You are signed in. An application is requesting access: <code>{scope}</code>.</p>\
         <form method=\"post\" action=\"{AUTHORIZE_PATH}\">\
         {hidden}\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\">\
         <p><button type=\"submit\" name=\"decision\" value=\"approve\">Allow</button> \
         <button type=\"submit\" name=\"decision\" value=\"deny\">Deny</button></p>\
         </form></body></html>",
        scope = html_escape(scope),
        hidden = hidden_authorize_fields(req),
        csrf = html_escape(csrf),
    )
}

/// A minimal error page (an unknown client / unregistered redirect / CSRF / server failure) — these
/// are NOT redirected to the client (we must not bounce to an unvalidated URI).
fn error_page(message: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>qfs — error</title></head>\
         <body><h1>Authorization error</h1><p>{}</p></body></html>",
        html_escape(message)
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use qfs_oauth::{pkce_challenge_s256, Jwks};
    use qfs_store::{FileSource, SqliteIdentityStore, SystemDb};
    use std::collections::BTreeMap;

    /// Open the System DB at `path` (file-backed so multiple connections share it), seed a local
    /// user, and build a full `OauthRoutes` with the live flow over that DB.
    fn routes_with_user(path: &std::path::Path, email: &str, password: &str) -> (OauthRoutes, i64) {
        // Seed a user via the identity store.
        let sys = SystemDb::open(&FileSource::new(path)).unwrap();
        let identity = SqliteIdentityStore::from_db(sys.into_db());
        let hash = qfs_identity::hash_password(&Secret::from(password)).unwrap();
        let user = identity.signup_local(email, &hash).unwrap();
        let uid = user.id.0;
        drop(identity);

        // Build the key store + the full routes over the same file DB.
        let key_store = {
            let conn = SystemDb::open(&FileSource::new(path))
                .unwrap()
                .into_db()
                .into_connection();
            OauthKeyStore::open_or_init(conn, &Secret::from("test-passphrase")).unwrap()
        };
        // Point the flow stores at THIS db by setting the config home env the openers read.
        let routes = build_routes_from_store(&key_store, "http://localhost:8787").unwrap();
        (routes, uid)
    }

    /// Set XDG_CONFIG_HOME so `open_system_db()` resolves to `dir/qfs/system.db`, returning that path.
    fn point_config_home(dir: &std::path::Path) -> std::path::PathBuf {
        std::env::set_var("XDG_CONFIG_HOME", dir);
        dir.join("qfs").join("system.db")
    }

    fn get(routes: &OauthRoutes, req: &HttpRequest) -> HttpResponse {
        routes.handle(req).expect("route handled")
    }

    fn header<'a>(resp: &'a HttpResponse, name: &str) -> Option<&'a str> {
        resp.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    // The env (XDG_CONFIG_HOME / QFS_OAUTH_ISSUER) the flow-store openers read is process-global;
    // serialize the file-backed tests via the crate-wide ENV_LOCK so they don't race each other — OR
    // the sibling `store.rs` tests, which also mutate `XDG_CONFIG_HOME` (one shared lock makes that
    // mutual exclusion cross-module, so the parallel harness is deterministic).
    use crate::ENV_LOCK;

    #[test]
    fn discovery_now_advertises_the_live_flow_endpoints() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = point_config_home(dir.path());
        let (routes, _uid) = routes_with_user(&path, "a@b.com", "password123");

        let asm: serde_json::Value =
            serde_json::from_slice(&get(&routes, &HttpRequest::new(Method::Get, ASM_PATH)).body)
                .unwrap();
        assert_eq!(
            asm["authorization_endpoint"],
            "http://localhost:8787/authorize"
        );
        assert_eq!(asm["token_endpoint"], "http://localhost:8787/token");
        assert_eq!(
            asm["registration_endpoint"],
            "http://localhost:8787/register"
        );
        assert_eq!(asm["grant_types_supported"][0], "authorization_code");
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn full_authorize_code_token_happy_path_with_pkce() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = point_config_home(dir.path());
        let (routes, uid) = routes_with_user(&path, "user@x.io", "hunter2hunter2");

        // 1. DCR: register a public client with one redirect URI.
        let reg_body = br#"{"redirect_uris":["https://app.example/cb"],"client_name":"Test"}"#;
        let reg = get(
            &routes,
            &HttpRequest {
                method: Method::Post,
                path: REGISTER_PATH.to_string(),
                query: BTreeMap::new(),
                headers: BTreeMap::new(),
                body: reg_body.to_vec(),
            },
        );
        assert_eq!(reg.status, 201);
        let reg_json: serde_json::Value = serde_json::from_slice(&reg.body).unwrap();
        let client_id = reg_json["client_id"].as_str().unwrap().to_string();
        assert!(
            reg_json.get("client_secret").is_none(),
            "public client: no secret"
        );

        // 2. Sign in + consent in one POST /authorize (no prior session → credentials path).
        let verifier = "a-high-entropy-pkce-verifier-0123456789-abcdefghijklmnop";
        let challenge = pkce_challenge_s256(verifier);
        let form = format!(
            "response_type=code&client_id={cid}&redirect_uri={ru}&scope=mcp%3Aread&state=st-123\
             &code_challenge={ch}&code_challenge_method=S256\
             &email=user%40x.io&password=hunter2hunter2&decision=approve",
            cid = client_id,
            ru = "https%3A%2F%2Fapp.example%2Fcb",
            ch = challenge,
        );
        let authz = get(
            &routes,
            &HttpRequest {
                method: Method::Post,
                path: AUTHORIZE_PATH.to_string(),
                query: BTreeMap::new(),
                headers: BTreeMap::new(),
                body: form.into_bytes(),
            },
        );
        assert_eq!(authz.status, 302, "approval redirects with a code");
        let location = header(&authz, "Location").unwrap();
        assert!(
            header(&authz, "Set-Cookie").is_some(),
            "a session was minted"
        );
        // Extract the code from the redirect.
        let code = location
            .split(['?', '&'])
            .find_map(|kv| kv.strip_prefix("code="))
            .unwrap()
            .to_string();
        assert!(
            location.contains("state=st-123"),
            "state echoed: {location}"
        );

        // 3. Exchange the code at /token with the PKCE verifier.
        let token_form = format!(
            "grant_type=authorization_code&code={code}&redirect_uri={ru}&client_id={cid}&code_verifier={ver}",
            code = code,
            ru = "https%3A%2F%2Fapp.example%2Fcb",
            cid = client_id,
            ver = verifier,
        );
        let tok = get(
            &routes,
            &HttpRequest {
                method: Method::Post,
                path: TOKEN_PATH.to_string(),
                query: BTreeMap::new(),
                headers: BTreeMap::new(),
                body: token_form.clone().into_bytes(),
            },
        );
        assert_eq!(tok.status, 200, "token issued: {}", tok.body_text());
        let tok_json: serde_json::Value = serde_json::from_slice(&tok.body).unwrap();
        assert_eq!(tok_json["token_type"], "Bearer");
        assert!(tok_json["refresh_token"].is_string());
        let access = tok_json["access_token"].as_str().unwrap();

        // 4. The access token verifies against the published JWKS and binds the user.
        let jwks: Jwks =
            serde_json::from_slice(&get(&routes, &HttpRequest::new(Method::Get, JWKS_PATH)).body)
                .unwrap();
        let claims = verify_jws(access, &jwks).unwrap();
        assert_eq!(claims["sub"], uid.to_string());
        assert_eq!(claims["aud"], "http://localhost:8787/mcp");
        assert_eq!(claims["iss"], "http://localhost:8787");
        assert_eq!(claims["scope"], "mcp:read");

        // 5. Replaying the SAME code is rejected (single-use).
        let replay = get(
            &routes,
            &HttpRequest {
                method: Method::Post,
                path: TOKEN_PATH.to_string(),
                query: BTreeMap::new(),
                headers: BTreeMap::new(),
                body: token_form.into_bytes(),
            },
        );
        assert_eq!(replay.status, 400);
        let replay_json: serde_json::Value = serde_json::from_slice(&replay.body).unwrap();
        assert_eq!(replay_json["error"], "invalid_grant");
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    /// Run register → authorize (sign-in) → token over `routes` for `email`/`password`, returning
    /// `(client_id, access_token, refresh_token)`. Factors the happy path so the refresh + MCP-gating
    /// tests can start from a freshly minted token pair.
    fn run_to_token(routes: &OauthRoutes, email: &str, password: &str) -> (String, String, String) {
        let reg = get(
            routes,
            &HttpRequest {
                method: Method::Post,
                path: REGISTER_PATH.to_string(),
                query: BTreeMap::new(),
                headers: BTreeMap::new(),
                body: br#"{"redirect_uris":["https://app.example/cb"]}"#.to_vec(),
            },
        );
        let client_id = serde_json::from_slice::<serde_json::Value>(&reg.body).unwrap()
            ["client_id"]
            .as_str()
            .unwrap()
            .to_string();
        let verifier = "a-high-entropy-pkce-verifier-0123456789-abcdefghijklmnop";
        let challenge = pkce_challenge_s256(verifier);
        let form = format!(
            "response_type=code&client_id={cid}&redirect_uri={ru}&scope=mcp%3Aread&state=st\
             &code_challenge={ch}&code_challenge_method=S256&email={em}&password={pw}&decision=approve",
            cid = client_id,
            ru = "https%3A%2F%2Fapp.example%2Fcb",
            ch = challenge,
            em = urlencode(email),
            pw = urlencode(password),
        );
        let authz = get(
            routes,
            &HttpRequest {
                method: Method::Post,
                path: AUTHORIZE_PATH.to_string(),
                query: BTreeMap::new(),
                headers: BTreeMap::new(),
                body: form.into_bytes(),
            },
        );
        let location = header(&authz, "Location").unwrap();
        let code = location
            .split(['?', '&'])
            .find_map(|kv| kv.strip_prefix("code="))
            .unwrap()
            .to_string();
        let token_form = format!(
            "grant_type=authorization_code&code={code}&redirect_uri={ru}&client_id={cid}&code_verifier={ver}",
            ru = "https%3A%2F%2Fapp.example%2Fcb",
            cid = client_id,
            ver = verifier,
        );
        let tok = get(
            routes,
            &HttpRequest {
                method: Method::Post,
                path: TOKEN_PATH.to_string(),
                query: BTreeMap::new(),
                headers: BTreeMap::new(),
                body: token_form.into_bytes(),
            },
        );
        assert_eq!(tok.status, 200, "token issued: {}", tok.body_text());
        let tj: serde_json::Value = serde_json::from_slice(&tok.body).unwrap();
        (
            client_id,
            tj["access_token"].as_str().unwrap().to_string(),
            tj["refresh_token"].as_str().unwrap().to_string(),
        )
    }

    fn token_post(routes: &OauthRoutes, form: String) -> HttpResponse {
        get(
            routes,
            &HttpRequest {
                method: Method::Post,
                path: TOKEN_PATH.to_string(),
                query: BTreeMap::new(),
                headers: BTreeMap::new(),
                body: form.into_bytes(),
            },
        )
    }

    #[test]
    fn refresh_grant_rotates_the_handle_and_mints_a_fresh_access_token() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = point_config_home(dir.path());
        let (routes, _uid) = routes_with_user(&path, "ref@x.io", "password1234");
        let (client_id, _access, refresh) = run_to_token(&routes, "ref@x.io", "password1234");

        // Exchange the refresh handle for a new access token + a ROTATED refresh handle.
        let resp = token_post(
            &routes,
            format!(
                "grant_type=refresh_token&refresh_token={r}&client_id={c}",
                r = refresh,
                c = client_id
            ),
        );
        assert_eq!(resp.status, 200, "refresh minted: {}", resp.body_text());
        let rj: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(rj["token_type"], "Bearer");
        let new_access = rj["access_token"].as_str().unwrap();
        let new_refresh = rj["refresh_token"].as_str().unwrap();
        assert_ne!(new_refresh, refresh, "the refresh handle was rotated");

        // The freshly minted access token verifies against the JWKS and binds the user/audience.
        let jwks: Jwks =
            serde_json::from_slice(&get(&routes, &HttpRequest::new(Method::Get, JWKS_PATH)).body)
                .unwrap();
        let claims = verify_jws(new_access, &jwks).unwrap();
        assert_eq!(claims["aud"], "http://localhost:8787/mcp");
        assert_eq!(claims["scope"], "mcp:read");

        // The NEW refresh handle works (rotation chain continues).
        let again = token_post(
            &routes,
            format!(
                "grant_type=refresh_token&refresh_token={r}&client_id={c}",
                r = new_refresh,
                c = client_id
            ),
        );
        assert_eq!(again.status, 200, "the rotated handle is usable");
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn a_reused_refresh_handle_is_rejected_invalid_grant() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = point_config_home(dir.path());
        let (routes, _uid) = routes_with_user(&path, "reuse@x.io", "password1234");
        let (client_id, _access, refresh) = run_to_token(&routes, "reuse@x.io", "password1234");

        // First use rotates (burns) the handle.
        let first = token_post(
            &routes,
            format!("grant_type=refresh_token&refresh_token={refresh}&client_id={client_id}"),
        );
        assert_eq!(first.status, 200);

        // Replaying the SAME (now-rotated) handle is rejected — single-use.
        let replay = token_post(
            &routes,
            format!("grant_type=refresh_token&refresh_token={refresh}&client_id={client_id}"),
        );
        assert_eq!(replay.status, 400);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&replay.body).unwrap()["error"],
            "invalid_grant"
        );
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn an_unknown_grant_type_is_unsupported() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = point_config_home(dir.path());
        let (routes, _uid) = routes_with_user(&path, "g@x.io", "password1234");
        let resp = token_post(&routes, "grant_type=client_credentials".to_string());
        assert_eq!(resp.status, 400);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&resp.body).unwrap()["error"],
            "unsupported_grant_type"
        );
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn the_minted_access_token_gates_the_mcp_endpoint_end_to_end() {
        use qfs_oauth::verify_access_token;
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = point_config_home(dir.path());
        let (routes, uid) = routes_with_user(&path, "mcp@x.io", "password1234");
        let (_client_id, access, _refresh) = run_to_token(&routes, "mcp@x.io", "password1234");

        // The bearer-validation material the binary lifts to gate /mcp.
        let v = routes.mcp_verification();
        assert_eq!(v.issuer, "http://localhost:8787");
        assert_eq!(v.audience, "http://localhost:8787/mcp");
        assert!(v.prm_url.ends_with("/.well-known/oauth-protected-resource"));

        // The freshly minted token verifies through the SAME pure primitive the authorizer uses.
        let now = now_unix();
        let verified = verify_access_token(&access, &v.jwks, &v.issuer, &v.audience, now)
            .expect("valid token");
        assert_eq!(verified.subject, uid.to_string());

        // A token for a DIFFERENT audience is rejected (audience-confusion guard).
        assert!(verify_access_token(
            &access,
            &v.jwks,
            &v.issuer,
            "http://localhost:8787/other",
            now
        )
        .is_err());
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn token_rejects_a_wrong_pkce_verifier_and_redirect_mismatch() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = point_config_home(dir.path());
        let (routes, _uid) = routes_with_user(&path, "p@x.io", "password1234");

        // Register + obtain a code (sign-in path) bound to challenge(verifier) + redirect.
        let reg = get(
            &routes,
            &HttpRequest {
                method: Method::Post,
                path: REGISTER_PATH.to_string(),
                query: BTreeMap::new(),
                headers: BTreeMap::new(),
                body: br#"{"redirect_uris":["https://app.example/cb"]}"#.to_vec(),
            },
        );
        let client_id = serde_json::from_slice::<serde_json::Value>(&reg.body).unwrap()
            ["client_id"]
            .as_str()
            .unwrap()
            .to_string();
        let verifier = "the-real-verifier-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let challenge = pkce_challenge_s256(verifier);
        let form = format!(
            "response_type=code&client_id={cid}&redirect_uri=https%3A%2F%2Fapp.example%2Fcb&scope=&state=s1\
             &code_challenge={ch}&code_challenge_method=S256&email=p%40x.io&password=password1234&decision=approve",
            cid = client_id,
            ch = challenge,
        );
        let authz = get(
            &routes,
            &HttpRequest {
                method: Method::Post,
                path: AUTHORIZE_PATH.to_string(),
                query: BTreeMap::new(),
                headers: BTreeMap::new(),
                body: form.into_bytes(),
            },
        );
        let location = header(&authz, "Location").unwrap();
        let code = location
            .split(['?', '&'])
            .find_map(|kv| kv.strip_prefix("code="))
            .unwrap()
            .to_string();

        // WRONG verifier → invalid_grant (and burns the code).
        let bad = get(
            &routes,
            &HttpRequest {
                method: Method::Post,
                path: TOKEN_PATH.to_string(),
                query: BTreeMap::new(),
                headers: BTreeMap::new(),
                body: format!(
                    "grant_type=authorization_code&code={code}&redirect_uri=https%3A%2F%2Fapp.example%2Fcb&client_id={cid}&code_verifier=WRONG-verifier",
                    code = code, cid = client_id
                ).into_bytes(),
            },
        );
        assert_eq!(bad.status, 400);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bad.body).unwrap()["error"],
            "invalid_grant"
        );
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn authorize_rejects_an_unregistered_redirect_uri_without_redirecting() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = point_config_home(dir.path());
        let (routes, _uid) = routes_with_user(&path, "r@x.io", "password1234");

        let reg = get(
            &routes,
            &HttpRequest {
                method: Method::Post,
                path: REGISTER_PATH.to_string(),
                query: BTreeMap::new(),
                headers: BTreeMap::new(),
                body: br#"{"redirect_uris":["https://app.example/cb"]}"#.to_vec(),
            },
        );
        let client_id = serde_json::from_slice::<serde_json::Value>(&reg.body).unwrap()
            ["client_id"]
            .as_str()
            .unwrap()
            .to_string();

        // A redirect_uri NOT in the allowlist → a 400 error PAGE, never a 302 to the attacker URI.
        let mut query = BTreeMap::new();
        query.insert("response_type".into(), "code".into());
        query.insert("client_id".into(), client_id);
        query.insert("redirect_uri".into(), "https://evil.example/steal".into());
        query.insert("scope".into(), "mcp:read".into());
        query.insert("state".into(), "s".into());
        query.insert("code_challenge".into(), "abc".into());
        query.insert("code_challenge_method".into(), "S256".into());
        let resp = get(
            &routes,
            &HttpRequest {
                method: Method::Get,
                path: AUTHORIZE_PATH.to_string(),
                query,
                headers: BTreeMap::new(),
                body: Vec::new(),
            },
        );
        assert_eq!(resp.status, 400);
        assert!(
            header(&resp, "Location").is_none(),
            "must NOT redirect to an unregistered URI"
        );
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn authorize_get_without_a_session_renders_the_signin_form() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = point_config_home(dir.path());
        let (routes, _uid) = routes_with_user(&path, "s@x.io", "password1234");

        let reg = get(
            &routes,
            &HttpRequest {
                method: Method::Post,
                path: REGISTER_PATH.to_string(),
                query: BTreeMap::new(),
                headers: BTreeMap::new(),
                body: br#"{"redirect_uris":["https://app.example/cb"]}"#.to_vec(),
            },
        );
        let client_id = serde_json::from_slice::<serde_json::Value>(&reg.body).unwrap()
            ["client_id"]
            .as_str()
            .unwrap()
            .to_string();
        let mut query = BTreeMap::new();
        query.insert("response_type".into(), "code".into());
        query.insert("client_id".into(), client_id);
        query.insert("redirect_uri".into(), "https://app.example/cb".into());
        query.insert("scope".into(), "mcp:read".into());
        query.insert("state".into(), "s".into());
        query.insert("code_challenge".into(), "abc".into());
        query.insert("code_challenge_method".into(), "S256".into());
        let resp = get(
            &routes,
            &HttpRequest {
                method: Method::Get,
                path: AUTHORIZE_PATH.to_string(),
                query,
                headers: BTreeMap::new(),
                body: Vec::new(),
            },
        );
        assert_eq!(resp.status, 200);
        let html = resp.body_text();
        assert!(html.contains("type=\"password\""), "renders a sign-in form");
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn register_rejects_a_malformed_redirect_uri() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = point_config_home(dir.path());
        let (routes, _uid) = routes_with_user(&path, "m@x.io", "password1234");
        let resp = get(
            &routes,
            &HttpRequest {
                method: Method::Post,
                path: REGISTER_PATH.to_string(),
                query: BTreeMap::new(),
                headers: BTreeMap::new(),
                body: br#"{"redirect_uris":["not-a-url"]}"#.to_vec(),
            },
        );
        assert_eq!(resp.status, 400);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&resp.body).unwrap()["error"],
            "invalid_redirect_uri"
        );
        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn issuer_derivation_prefers_override_then_loopback_localhost() {
        let _g = ENV_LOCK.lock().unwrap();
        let addr: SocketAddr = "127.0.0.1:8787".parse().unwrap();
        assert_eq!(issuer_from_addr(addr), "http://localhost:8787");
        let public: SocketAddr = "10.0.0.5:9000".parse().unwrap();
        assert_eq!(issuer_from_addr(public), "http://10.0.0.5:9000");
        std::env::set_var("QFS_OAUTH_ISSUER", "https://qfs.example.com/");
        assert_eq!(issuer_from_addr(addr), "https://qfs.example.com");
        std::env::remove_var("QFS_OAUTH_ISSUER");
    }

    #[test]
    fn non_get_and_unknown_paths_fall_through() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = point_config_home(dir.path());
        let (routes, _uid) = routes_with_user(&path, "f@x.io", "password1234");
        // A POST to a well-known discovery path falls through (read-only discovery).
        assert!(routes
            .handle(&HttpRequest::new(Method::Post, JWKS_PATH))
            .is_none());
        // An unrelated path falls through.
        assert!(routes
            .handle(&HttpRequest::new(Method::Get, "/status"))
            .is_none());
        std::env::remove_var("XDG_CONFIG_HOME");
    }
}
