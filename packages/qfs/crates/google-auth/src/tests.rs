//! Real tests for the Google OAuth + multi-account auth base — **no live Google, no network**.
//! Every wire exchange runs against the local [`MockExchange`] transport (scripted responses +
//! recorded requests, mirroring the t18 mock) and the t27 [`qfs_secrets::InMemoryStore`]. The
//! interactive consent step is modeled (the redirect-parsing machinery is tested directly); the
//! non-interactive token machinery is exercised end to end.

use std::sync::Arc;
use std::time::Duration;

use qfs_secrets::{InMemoryStore, Secret, Secrets};

use super::*;

/// A planted credential value, unmistakable if it ever surfaces on a log/error surface.
const PLANTED: &str = "PLANTED-LEAK-CANARY-google-3a2b1c0d";

fn token_body(access: &str, refresh: Option<&str>, expires_in: u64) -> Vec<u8> {
    let mut map = serde_json::Map::new();
    map.insert("access_token".into(), serde_json::json!(access));
    map.insert("token_type".into(), serde_json::json!("Bearer"));
    map.insert("expires_in".into(), serde_json::json!(expires_in));
    if let Some(r) = refresh {
        map.insert("refresh_token".into(), serde_json::json!(r));
    }
    serde_json::to_vec(&serde_json::Value::Object(map)).unwrap()
}

fn userinfo_body(email: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "email": email, "sub": "12345" })).unwrap()
}

fn ok_json(body: Vec<u8>) -> HttpResponse {
    HttpResponse::new(200, body).header("Content-Type", "application/json")
}

fn oauth_with(mock: MockExchange, scopes: Vec<String>) -> (OAuthClient, Arc<MockExchange>) {
    let mock = Arc::new(mock);
    let client = OAuthClient::new(
        "client-id-123.apps.googleusercontent.com",
        Secret::from(PLANTED), // the client_secret is the planted canary
        scopes,
        Arc::clone(&mock) as Arc<dyn HttpExchange>,
    );
    (client, mock)
}

// ---- Auth URL / redirect-uri shape (the localhost gotcha) -------------------------------

/// THE load-bearing detail: the advertised redirect URI host is `localhost`, NOT `127.0.0.1`.
#[test]
fn redirect_uri_host_is_localhost_not_ip() {
    let uri = OAuthClient::redirect_uri(54321);
    assert_eq!(uri, "http://localhost:54321");
    assert!(uri.contains("localhost"));
    assert!(
        !uri.contains("127.0.0.1"),
        "redirect URI must not use the loopback IP (silent-consent stall): {uri}"
    );
    assert_eq!(LOOPBACK_REDIRECT_HOST, "localhost");
}

/// The auth URL carries the loopback `localhost` redirect, the offline/consent params that
/// guarantee a refresh token, the caller's scopes, and the state.
#[test]
fn auth_url_carries_localhost_offline_consent_and_scopes() {
    let (oauth, _mock) = oauth_with(
        MockExchange::new(),
        vec!["https://www.googleapis.com/auth/gmail.readonly".to_string()],
    );
    let redirect = OAuthClient::redirect_uri(40000);
    let url = oauth.build_auth_url(&redirect, "STATE-XYZ").unwrap();

    assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
    // Redirect uses localhost, not the IP — assert on the encoded form.
    assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A40000"));
    assert!(!url.contains("127.0.0.1"));
    assert!(url.contains("access_type=offline"));
    assert!(url.contains("prompt=consent"));
    assert!(url.contains("gmail.readonly"));
    assert!(url.contains("state=STATE-XYZ"));
    assert!(url.contains("response_type=code"));
}

// ---- Token exchange request shape -------------------------------------------------------

/// The auth-code -> token exchange POSTs the correct form to the token endpoint, and the
/// returned access token + refresh token are captured. Asserts the request *shape* (method,
/// URL, content-type, form fields) against the recorded request.
#[test]
fn exchange_code_posts_correct_form_and_captures_tokens() {
    let mock = MockExchange::new().with_response(ok_json(token_body(
        "access-AAA",
        Some("refresh-RRR"),
        3600,
    )));
    let (oauth, mock) = oauth_with(mock, vec!["scope.a".to_string()]);
    let redirect = OAuthClient::redirect_uri(8080);

    let (access, refresh) = oauth.exchange_code("auth-code-123", &redirect, 0).unwrap();
    assert_eq!(access.bearer(), Some("access-AAA"));
    assert_eq!(refresh.expose_str(), Some("refresh-RRR"));

    let recorded = mock.recorded();
    assert_eq!(recorded.len(), 1);
    let req = &recorded[0];
    assert_eq!(req.method, HttpMethod::Post);
    assert_eq!(req.url, "https://oauth2.googleapis.com/token");
    assert_eq!(
        req.header_value("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    let body = String::from_utf8(req.body.clone().unwrap()).unwrap();
    assert!(body.contains("grant_type=authorization_code"));
    assert!(body.contains("code=auth-code-123"));
    assert!(body.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8080"));
    assert!(body.contains("client_id=client-id-123"));
    // The client_secret IS sent on the wire (correct) ...
    assert!(body.contains("client_secret="));
}

/// A token body with no refresh_token (re-consent without prompt=consent) is a typed error,
/// not a silent success.
#[test]
fn exchange_without_refresh_token_is_invalid() {
    let mock = MockExchange::new().with_response(ok_json(token_body("access-AAA", None, 3600)));
    let (oauth, _mock) = oauth_with(mock, vec![]);
    let err = oauth
        .exchange_code("c", &OAuthClient::redirect_uri(1), 0)
        .unwrap_err();
    assert_eq!(err.code(), "auth_invalid");
}

// ---- Refresh flow + request shape -------------------------------------------------------

/// The refresh flow POSTs grant_type=refresh_token with the stored refresh token and returns a
/// fresh access token.
#[test]
fn refresh_posts_refresh_grant_and_returns_new_access_token() {
    let mock = MockExchange::new().with_response(ok_json(token_body("access-NEW", None, 3600)));
    let (oauth, mock) = oauth_with(mock, vec![]);
    let refresh = Secret::from("refresh-RRR");

    let access = oauth.refresh_access_token(&refresh, 0).unwrap();
    assert_eq!(access.bearer(), Some("access-NEW"));

    let req = &mock.recorded()[0];
    let body = String::from_utf8(req.body.clone().unwrap()).unwrap();
    assert!(body.contains("grant_type=refresh_token"));
    assert!(body.contains("refresh_token=refresh-RRR"));
}

/// invalid_grant from the refresh endpoint maps to AuthError::TokenRefresh, NOT a panic / a
/// generic error — and flags re-authorize-required.
#[test]
fn invalid_grant_maps_to_typed_token_refresh_error() {
    let body = serde_json::to_vec(&serde_json::json!({
        "error": "invalid_grant",
        "error_description": "Token has been expired or revoked."
    }))
    .unwrap();
    let mock = MockExchange::new().with_response(HttpResponse::new(400, body));
    let (oauth, _mock) = oauth_with(mock, vec![]);

    let err = oauth
        .refresh_access_token(&Secret::from("revoked"), 0)
        .unwrap_err();
    assert_eq!(err.code(), "auth_token_refresh");
    assert!(matches!(err, AuthError::TokenRefresh { ref reason } if reason == "invalid_grant"));
    assert!(err.is_reauthorize_required());
}

// ---- Profile email lookup ---------------------------------------------------------------

/// fetch_profile_email returns the userinfo email and sends the bearer.
#[test]
fn fetch_profile_email_returns_email_with_bearer() {
    let mock = MockExchange::new().with_response(ok_json(userinfo_body("alice@example.com")));
    let (oauth, mock) = oauth_with(mock, vec![]);
    let access = AccessToken::new(Secret::from("access-AAA"), u128::MAX);

    let email = oauth.fetch_profile_email(&access).unwrap();
    assert_eq!(email, "alice@example.com");

    let req = &mock.recorded()[0];
    assert_eq!(req.url, "https://www.googleapis.com/oauth2/v3/userinfo");
    assert_eq!(req.header_value("authorization"), Some("Bearer access-AAA"));
}

// ---- StoredTokenSource: cache + refresh-on-expiry ---------------------------------------

fn store_with_refresh(email: &str, refresh: &str) -> Arc<dyn Secrets> {
    let store = InMemoryStore::new();
    let key = refresh_token_key(email).unwrap();
    store.put(&key, Secret::from(refresh)).unwrap();
    Arc::new(store)
}

/// access_token returns a cached token before expiry and triggers EXACTLY ONE refresh after
/// expiry (golden test against the mock — no live creds).
#[test]
fn stored_source_caches_then_refreshes_exactly_once_on_expiry() {
    // Two scripted refresh responses; we assert exactly the right number are consumed.
    let mock = Arc::new(
        MockExchange::new()
            .with_response(ok_json(token_body("access-1", None, 3600)))
            .with_response(ok_json(token_body("access-2", None, 3600))),
    );
    let oauth = OAuthClient::new(
        "cid",
        Secret::from(PLANTED),
        vec![],
        Arc::clone(&mock) as Arc<dyn HttpExchange>,
    );
    let clock = Arc::new(ManualClock::new());
    let store = store_with_refresh("bob@example.com", "refresh-bob");
    let src = StoredTokenSource::with_clock(
        "bob@example.com",
        store,
        oauth,
        Arc::clone(&clock) as Arc<dyn Clock>,
    );

    // First call: mints access-1 (one refresh request).
    assert_eq!(src.access_token().unwrap().bearer(), Some("access-1"));
    // Second call before expiry: cached, no new request.
    assert_eq!(src.access_token().unwrap().bearer(), Some("access-1"));
    assert_eq!(mock.recorded().len(), 1, "no refresh while cached");

    // Advance past the (3600s - 60s skew) lifetime: now expired -> exactly one more refresh.
    clock.advance(Duration::from_secs(3600));
    assert_eq!(src.access_token().unwrap().bearer(), Some("access-2"));
    assert_eq!(mock.recorded().len(), 2, "exactly one refresh after expiry");
}

/// A missing refresh token in the store surfaces a typed Store error (not a panic).
#[test]
fn stored_source_missing_refresh_token_is_store_error() {
    let mock = Arc::new(MockExchange::new());
    let oauth = OAuthClient::new(
        "cid",
        Secret::from(PLANTED),
        vec![],
        mock as Arc<dyn HttpExchange>,
    );
    let store: Arc<dyn Secrets> = Arc::new(InMemoryStore::new());
    let src = StoredTokenSource::new("nobody@example.com", store, oauth);
    let err = src.access_token().unwrap_err();
    assert_eq!(err.code(), "auth_store");
}

// ---- GoogleApiClient: 401 -> refresh -> retry -------------------------------------------

/// A 401 from a Google API call triggers exactly one token refresh + one retry; the retried
/// request carries the refreshed bearer.
#[test]
fn api_client_refreshes_and_retries_once_on_401() {
    // Sequence: (1) StoredTokenSource initial refresh -> access-1;
    //           (2) API call with access-1 -> 401;
    //           (3) refresh -> access-2;
    //           (4) retry API call with access-2 -> 200.
    let mock = Arc::new(
        MockExchange::new()
            .with_response(ok_json(token_body("access-1", None, 3600))) // initial mint
            .with_response(HttpResponse::new(401, b"unauthorized".to_vec())) // API 401
            .with_response(ok_json(token_body("access-2", None, 3600))) // refresh
            .with_response(ok_json(b"{\"ok\":true}".to_vec())), // retry 200
    );
    let oauth = OAuthClient::new(
        "cid",
        Secret::from(PLANTED),
        vec![],
        Arc::clone(&mock) as Arc<dyn HttpExchange>,
    );
    let clock = Arc::new(ManualClock::new());
    let store = store_with_refresh("carol@example.com", "refresh-carol");
    let src: Arc<dyn TokenSource> = Arc::new(StoredTokenSource::with_clock(
        "carol@example.com",
        store,
        oauth,
        clock,
    ));
    let api = GoogleApiClient::new(Arc::clone(&mock) as Arc<dyn HttpExchange>, src);

    let req = HttpRequest::new(
        HttpMethod::Get,
        "https://gmail.googleapis.com/gmail/v1/users/me/messages",
    );
    let resp = api.send(&req).unwrap();
    assert_eq!(resp.status, 200);

    let recorded = mock.recorded();
    // 4 wire exchanges total (mint, 401 api, refresh, retry api).
    assert_eq!(recorded.len(), 4);
    // The first API attempt used access-1; the retry used the refreshed access-2.
    assert_eq!(
        recorded[1].header_value("authorization"),
        Some("Bearer access-1")
    );
    assert_eq!(
        recorded[3].header_value("authorization"),
        Some("Bearer access-2")
    );
}

/// A non-401 status (e.g. 404) is returned to the caller, NOT treated as an auth failure, and
/// triggers no refresh/retry.
#[test]
fn api_client_passes_through_non_401_without_retry() {
    let mock = Arc::new(
        MockExchange::new()
            .with_response(ok_json(token_body("access-1", None, 3600)))
            .with_response(HttpResponse::new(404, b"not found".to_vec())),
    );
    let oauth = OAuthClient::new(
        "cid",
        Secret::from(PLANTED),
        vec![],
        Arc::clone(&mock) as Arc<dyn HttpExchange>,
    );
    let store = store_with_refresh("dave@example.com", "refresh-dave");
    let src: Arc<dyn TokenSource> = Arc::new(StoredTokenSource::with_clock(
        "dave@example.com",
        store,
        oauth,
        Arc::new(ManualClock::new()),
    ));
    let api = GoogleApiClient::new(Arc::clone(&mock) as Arc<dyn HttpExchange>, src);
    let req = HttpRequest::new(HttpMethod::Get, "https://api/x");
    let resp = api.send(&req).unwrap();
    assert_eq!(resp.status, 404);
    assert_eq!(mock.recorded().len(), 2, "no refresh/retry on a 404");
}

// ---- Multi-account resolve --------------------------------------------------------------

/// Two distinct account emails are stored independently and resolve back to two distinct
/// TokenSources, each minting its own account's access token.
#[test]
fn two_accounts_resolve_to_independent_token_sources() {
    let store = InMemoryStore::new();
    store
        .put(
            &refresh_token_key("a@example.com").unwrap(),
            Secret::from("refresh-A"),
        )
        .unwrap();
    store
        .put(
            &refresh_token_key("b@example.com").unwrap(),
            Secret::from("refresh-B"),
        )
        .unwrap();
    let store: Arc<dyn Secrets> = Arc::new(store);

    // Account A: mock returns access-A; assert its refresh form carried refresh-A.
    let mock_a =
        Arc::new(MockExchange::new().with_response(ok_json(token_body("access-A", None, 3600))));
    let oauth_a = OAuthClient::new(
        "cid",
        Secret::from(PLANTED),
        vec![],
        Arc::clone(&mock_a) as Arc<dyn HttpExchange>,
    );
    let src_a = StoredTokenSource::new("a@example.com", Arc::clone(&store), oauth_a);
    assert_eq!(src_a.access_token().unwrap().bearer(), Some("access-A"));
    assert!(
        String::from_utf8(mock_a.recorded()[0].body.clone().unwrap())
            .unwrap()
            .contains("refresh_token=refresh-A")
    );

    // Account B: independent source, independent secret.
    let mock_b =
        Arc::new(MockExchange::new().with_response(ok_json(token_body("access-B", None, 3600))));
    let oauth_b = OAuthClient::new(
        "cid",
        Secret::from(PLANTED),
        vec![],
        Arc::clone(&mock_b) as Arc<dyn HttpExchange>,
    );
    let src_b = StoredTokenSource::new("b@example.com", store, oauth_b);
    assert_eq!(src_b.access_token().unwrap().bearer(), Some("access-B"));
    assert!(
        String::from_utf8(mock_b.recorded()[0].body.clone().unwrap())
            .unwrap()
            .contains("refresh_token=refresh-B")
    );

    assert_eq!(src_a.account_email(), "a@example.com");
    assert_eq!(src_b.account_email(), "b@example.com");
}

// ---- Scope handling ---------------------------------------------------------------------

/// Scopes are caller-supplied (this crate is scope-agnostic): each driver passes its own
/// minimum set, and it lands verbatim in the auth URL.
#[test]
fn scopes_are_caller_supplied_and_flow_into_the_auth_url() {
    let gmail_scopes = vec![
        "https://www.googleapis.com/auth/gmail.readonly".to_string(),
        "https://www.googleapis.com/auth/gmail.send".to_string(),
    ];
    let (oauth, _m) = oauth_with(MockExchange::new(), gmail_scopes.clone());
    assert_eq!(oauth.scopes(), gmail_scopes.as_slice());
    let url = oauth
        .build_auth_url(&OAuthClient::redirect_uri(1), "s")
        .unwrap();
    assert!(url.contains("gmail.readonly"));
    assert!(url.contains("gmail.send"));

    // Scope grant/deny reuses the t27 surface: a driver checks held vs required scopes.
    let grant = qfs_secrets::grant_scopes(
        &["https://www.googleapis.com/auth/gmail.readonly".to_string()],
        &gmail_scopes,
    )
    .unwrap();
    assert_eq!(grant.granted.len(), 1);
    let deny = qfs_secrets::grant_scopes(
        &["https://www.googleapis.com/auth/drive".to_string()],
        &gmail_scopes,
    )
    .unwrap_err();
    assert_eq!(deny.code(), "scope_denied");
}

// ---- Redirect parsing / state validation (models the interactive consent) ---------------

/// A valid redirect with a matching state yields the code; a mismatched state is rejected
/// (CSRF guard); a denial is a typed Denied.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn redirect_parsing_validates_state_and_extracts_code() {
    use crate::authorize::parse_redirect_request;
    let req = "GET /?code=THE-CODE&state=GOOD HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert_eq!(parse_redirect_request(req, "GOOD").unwrap(), "THE-CODE");

    // Wrong state -> StateMismatch, code never returned.
    let err = parse_redirect_request(req, "EXPECTED-OTHER").unwrap_err();
    assert_eq!(err.code(), "auth_state_mismatch");

    // User declined -> Denied.
    let denied = "GET /?error=access_denied&state=GOOD HTTP/1.1\r\n\r\n";
    assert_eq!(
        parse_redirect_request(denied, "GOOD").unwrap_err().code(),
        "auth_denied"
    );
}

// ---- Secret safety (the headline redaction invariant) -----------------------------------

/// THE redaction invariant: no secret (client secret, code, access/refresh token) appears in
/// any error Display/Debug or in a token/account DTO's Debug. The client_secret here is the
/// planted canary; drive every error + DTO surface and assert it never leaks.
#[test]
fn no_secret_appears_in_any_error_or_dto_surface() {
    let mut surfaces: Vec<String> = Vec::new();

    // 1. AccessToken / GoogleAccount Debug — values are the planted canary.
    let access = AccessToken::new(Secret::from(PLANTED), 0);
    let account = GoogleAccount::new("user@example.com", Secret::from(PLANTED));
    surfaces.push(format!("{access:?}"));
    surfaces.push(format!("{account:?}"));

    // 2. Every AuthError variant's Display + Debug.
    let errs = vec![
        AuthError::Denied,
        AuthError::Timeout,
        AuthError::StateMismatch,
        AuthError::Network {
            endpoint: "token",
            code: "http_transport",
        },
        AuthError::TokenRefresh {
            reason: "invalid_grant".to_string(),
        },
        AuthError::ProfileLookup {
            reason: "userinfo status 500".to_string(),
        },
        AuthError::Store {
            code: "secret_not_found",
        },
        AuthError::Invalid {
            reason: "bad config".to_string(),
        },
    ];
    for e in &errs {
        surfaces.push(format!("{e}"));
        surfaces.push(format!("{e:?}"));
    }

    // 3. An error produced by an actual failing exchange where the body echoes nothing secret,
    //    but the client_secret IS in play on the wire.
    let mock = MockExchange::new().with_response(HttpResponse::new(
        400,
        serde_json::to_vec(&serde_json::json!({"error": "invalid_grant"})).unwrap(),
    ));
    let (oauth, _m) = oauth_with(mock, vec![PLANTED.to_string()]);
    let err = oauth
        .refresh_access_token(&Secret::from(PLANTED), 0)
        .unwrap_err();
    surfaces.push(format!("{err}"));
    surfaces.push(format!("{err:?}"));

    for s in &surfaces {
        assert!(
            !s.contains(PLANTED),
            "SECRET LEAK: planted value surfaced in: {s}"
        );
        assert!(
            !s.contains("3a2b1c0d"),
            "SECRET LEAK: fragment surfaced in: {s}"
        );
    }
}

/// The email->account-id encoding is injective and reversible: distinct emails map to distinct
/// (t27-valid) keys, and decode recovers the original. This underpins multi-account keying.
#[test]
fn email_account_key_encoding_round_trips_and_is_distinct() {
    for email in ["alice@example.com", "a.b+tag@sub.example.co.jp", "x@y"] {
        let key = refresh_token_key(email).unwrap();
        // The stored ConnectionId is t27-valid (no '@', '/', whitespace).
        let acct = key.connection.as_str();
        assert!(!acct.contains('@'));
        assert!(!acct.contains('/'));
        assert!(!acct.chars().any(char::is_whitespace));
        // ... and decodes back to the original email.
        assert_eq!(decode_account_email(acct), email);
    }
    // Distinct emails -> distinct keys (no collision).
    assert_ne!(
        refresh_token_key("a@b.com").unwrap().connection,
        refresh_token_key("a%40b.com").unwrap().connection
    );
    // Empty email is rejected, not silently keyed.
    assert_eq!(refresh_token_key("").unwrap_err().code(), "auth_invalid");
}

/// The error -> code mapping is stable (AI-legible recovery).
#[test]
fn auth_error_codes_are_stable() {
    assert_eq!(AuthError::Denied.code(), "auth_denied");
    assert_eq!(AuthError::Timeout.code(), "auth_timeout");
    assert_eq!(AuthError::StateMismatch.code(), "auth_state_mismatch");
    assert_eq!(
        AuthError::TokenRefresh { reason: "x".into() }.code(),
        "auth_token_refresh"
    );
}
