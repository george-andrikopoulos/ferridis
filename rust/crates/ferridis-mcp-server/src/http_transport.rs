//! HTTP transport — MCP Streamable HTTP at `POST /mcp` plus an
//! OAuth 2.1 authorization-server surface.
//!
//! This is the path claude.ai's "custom MCP connector" needs: a real
//! HTTPS endpoint with an OAuth handshake out front. Static-bearer
//! mode is also supported for the simpler call-it-from-curl path,
//! but claude.ai only ever takes the OAuth route.
//!
//! # Surfaces
//!
//! - `POST /mcp` — MCP Streamable HTTP, JSON-RPC in / JSON-RPC out.
//!   `Authorization: Bearer <token>` required; the token may be
//!   either an operator-configured static bearer (if `--bearer-token`
//!   was supplied) or an OAuth-issued access token (if
//!   `--oauth-public-base-url` was supplied). Either path alone is
//!   enough to start the publisher; both can coexist.
//! - `GET /.well-known/oauth-authorization-server` — RFC 8414
//!   metadata. Only served when OAuth is enabled.
//! - `POST /register` — RFC 7591 Dynamic Client Registration.
//! - `GET /authorize` — OAuth 2.1 authorization endpoint. Issues a
//!   one-shot `code` and 302-redirects to the caller's
//!   `redirect_uri`. Auto-approves; see [`crate::oauth_server`].
//! - `POST /token` — exchanges a code for an access token after
//!   PKCE S256 verification.
//!
//! Per-session state is not tracked on `/mcp`: every dispatch goes
//! through the same shared [`Server`], so concurrent callers are
//! safe. OAuth state (clients, codes, tokens) lives in
//! [`crate::oauth_server`].
//!
//! # Why a separate transport
//!
//! The handler core ([`Server::handle_request`] /
//! [`handle_notification`]) is transport-agnostic: it consumes parsed
//! [`Inbound`] values and returns [`Response`] values. The stdio loop
//! framing (line-delimited JSON over stdin/stdout) and the HTTP
//! framing here are both thin wrappers around that core; OAuth lives
//! alongside but does not entangle the MCP path.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Form;
use axum::Json;
use axum::Router;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response as HttpResponse};
use axum::routing::{get, post};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::oauth_server::{
    AuthorizeRequest, OAuthError, OAuthServer, RegisterRequest, TokenRequest,
};
use crate::protocol::{Inbound, Response, codes};
use crate::server::Server;

/// Configuration for [`serve_http`]. At least one of `bearer` /
/// `oauth` must be Some; the binary enforces that at arg-parse time.
pub struct HttpConfig {
    /// Address to bind, e.g. `127.0.0.1:7824`.
    pub bind: SocketAddr,
    /// Operator-configured static bearer. Accepted on `/mcp` when
    /// set. When `None`, only OAuth-issued tokens are honoured.
    pub bearer: Option<SecretString>,
    /// OAuth authorization server. When `Some`, the OAuth endpoints
    /// (`/authorize`, `/token`, `/register`, `/.well-known/…`) are
    /// served and OAuth-issued tokens are honoured on `/mcp`.
    pub oauth: Option<Arc<OAuthServer>>,
}

#[derive(Clone)]
struct AppState {
    server: Arc<Server>,
    bearer: Option<Arc<SecretString>>,
    oauth: Option<Arc<OAuthServer>>,
}

/// Bind to `cfg.bind` and serve forever. Returns only on listener or
/// serve failure (or process shutdown).
pub async fn serve_http(server: Arc<Server>, cfg: HttpConfig) -> std::io::Result<()> {
    let oauth_enabled = cfg.oauth.is_some();
    let bearer_enabled = cfg.bearer.is_some();
    let state = AppState {
        server,
        bearer: cfg.bearer.map(Arc::new),
        oauth: cfg.oauth,
    };

    // MCP Streamable HTTP is served at both `/mcp` and the root
    // path. claude.ai's Custom Connector flow assumes the connector
    // URL itself is the MCP endpoint and POSTs JSON-RPC to it
    // directly; that means a connector URL of `https://<tunnel>/`
    // (which is what claude.ai derives OAuth discovery from) only
    // works if `POST /` also dispatches MCP. Older MCP hosts that
    // pin the endpoint at `/mcp` keep working unchanged.
    let mut app = Router::new()
        .route("/mcp", post(handle_mcp))
        .route("/", post(handle_mcp));
    if oauth_enabled {
        app = app
            .route(
                "/.well-known/oauth-authorization-server",
                get(handle_metadata),
            )
            .route("/register", post(handle_register))
            .route("/authorize", get(handle_authorize))
            .route("/token", post(handle_token));
    }
    // Fallback: any request to an unmapped path is logged with
    // method + path + bearer-presence. Silent 404s on the MCP
    // surface have already cost us one debug round-trip
    // (claude.ai's connector UI reports a generic "Authorization
    // failed" when the MCP POST itself 404s post-token-issuance);
    // make the failure mode self-diagnosing next time.
    let app = app.fallback(log_unknown_route).with_state(state);

    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    info!(
        addr = %listener.local_addr()?,
        bearer = bearer_enabled,
        oauth = oauth_enabled,
        "ferridis-mcp-server HTTP transport listening"
    );
    axum::serve(listener, app).await
}

// ---- MCP endpoint ----------------------------------------------

/// One MCP request handler — auth, parse, dispatch, serialise.
async fn handle_mcp(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> HttpResponse {
    if !authorize_mcp_request(&headers, &state) {
        warn!("rejected MCP request: missing or invalid Authorization");
        return (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            r#"{"error":"missing or invalid bearer token"}"#,
        )
            .into_response();
    }

    let parsed: Result<Inbound, _> = serde_json::from_str(&body);
    match parsed {
        Ok(msg) if !msg.is_valid() => {
            let id = msg.id.clone().unwrap_or(Value::Null);
            let resp = Response::error(id, codes::INVALID_REQUEST, "jsonrpc field must be \"2.0\"");
            Json(resp).into_response()
        }
        Ok(msg) if msg.is_notification() => {
            debug!(method = %msg.method, "HTTP MCP notification");
            state.server.handle_notification(&msg).await;
            StatusCode::ACCEPTED.into_response()
        }
        Ok(msg) => {
            debug!(method = %msg.method, "HTTP MCP request");
            let response = state.server.handle_request(msg).await;
            Json(response).into_response()
        }
        Err(e) => {
            let resp =
                Response::error(Value::Null, codes::PARSE_ERROR, format!("parse error: {e}"));
            Json(resp).into_response()
        }
    }
}

/// Accept the request iff:
/// - the `Authorization: Bearer …` header is present, AND
/// - the token matches the static bearer (when configured), OR
/// - the token is a live OAuth-issued access token (when OAuth is
///   enabled).
///
/// Static and OAuth tokens are accepted on the same surface; the
/// difference is just where they came from. Both auth paths use the
/// same constant-time comparison shape so timing doesn't reveal
/// which check matched.
fn authorize_mcp_request(headers: &HeaderMap, state: &AppState) -> bool {
    let supplied = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    if supplied.is_empty() {
        return false;
    }
    if let Some(bearer) = &state.bearer
        && constant_time_eq(supplied.as_bytes(), bearer.expose_secret().as_bytes())
    {
        return true;
    }
    if let Some(oauth) = &state.oauth
        && oauth.validate_access_token(supplied).is_some()
    {
        return true;
    }
    false
}

// ---- OAuth endpoints -------------------------------------------

async fn handle_metadata(State(state): State<AppState>) -> HttpResponse {
    let Some(oauth) = &state.oauth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Json(oauth.metadata()).into_response()
}

async fn handle_register(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> HttpResponse {
    let Some(oauth) = &state.oauth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let resp = oauth.register_client(body);
    (StatusCode::CREATED, Json(resp)).into_response()
}

async fn handle_authorize(
    State(state): State<AppState>,
    Query(req): Query<AuthorizeRequest>,
) -> HttpResponse {
    let Some(oauth) = &state.oauth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let redirect_uri = req.redirect_uri.clone();
    let supplied_state = req.state.clone();
    match oauth.start_authorize(req) {
        Ok(code) => {
            // RFC 6749 §4.1.2: build the redirect with `code=` (and
            // `state=` if the caller supplied one). Use the typed
            // `url::Url` builder so we don't have to manually
            // percent-encode.
            let mut url = match url::Url::parse(&redirect_uri) {
                Ok(u) => u,
                Err(e) => return oauth_html_error(format!("invalid redirect_uri: {e}")),
            };
            url.query_pairs_mut().append_pair("code", &code);
            if let Some(s) = supplied_state {
                url.query_pairs_mut().append_pair("state", &s);
            }
            info!(target = %url, "OAuth: redirecting after auto-approve");
            Redirect::to(url.as_str()).into_response()
        }
        Err(e) => {
            // If we have a redirect_uri we trust, return the error in
            // the redirect (per OAuth §4.1.2.1); otherwise surface an
            // HTML page so the operator can see what went wrong. We
            // don't yet trust the redirect_uri at this point in an
            // error case (start_authorize may have rejected it
            // exactly because it's untrusted), so always render HTML.
            oauth_html_error(format!("{}: {e}", e.oauth_code()))
        }
    }
}

async fn handle_token(
    State(state): State<AppState>,
    Form(req): Form<TokenRequest>,
) -> HttpResponse {
    let Some(oauth) = &state.oauth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match oauth.exchange_code(req) {
        Ok(tok) => Json(tok).into_response(),
        Err(e) => oauth_json_error(e),
    }
}

fn oauth_json_error(e: OAuthError) -> HttpResponse {
    let body = serde_json::json!({
        "error": e.oauth_code(),
        "error_description": e.to_string(),
    });
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
    (status, Json(body)).into_response()
}

fn oauth_html_error(detail: String) -> HttpResponse {
    // Plain text rather than HTML to avoid any chance of an XSS
    // vector through reflected error details.
    (
        StatusCode::BAD_REQUEST,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        format!("OAuth error: {detail}"),
    )
        .into_response()
}

// ---- Fallback logging ------------------------------------------

/// 404-with-WARN-log fallback. Records `method` + `path` + whether an
/// `Authorization` header was present so an operator inspecting
/// `journalctl --user -u ferridis-mcp-http` can immediately see what
/// shape of request we missed.
async fn log_unknown_route(req: Request) -> HttpResponse {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let has_auth = req
        .headers()
        .contains_key(axum::http::header::AUTHORIZATION);
    let ua = req
        .headers()
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    warn!(
        method = %method,
        path = %path,
        has_authorization = has_auth,
        user_agent = %ua,
        "HTTP request to unmapped route (404)"
    );
    (
        StatusCode::NOT_FOUND,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        format!(r#"{{"error":"not_found","detail":"no handler for {method} {path}"}}"#),
    )
        .into_response()
}

// ---- Constant-time comparison ----------------------------------

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_bearer(b: &str) -> AppState {
        AppState {
            server: Arc::new(dummy_server()),
            bearer: Some(Arc::new(SecretString::new(b.to_string()))),
            oauth: None,
        }
    }

    fn dummy_server() -> Server {
        // Empty catalogue; no client calls go through these tests.
        let client = std::sync::Arc::new(ferridis_client::Client::ephemeral());
        Server::new(client, crate::tools::ToolCatalogue::default(), None)
    }

    #[test]
    fn constant_time_eq_matches_basic_cases() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn authorize_mcp_request_accepts_static_bearer() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer abc123"),
        );
        let s = state_with_bearer("abc123");
        assert!(authorize_mcp_request(&h, &s));
    }

    #[test]
    fn authorize_mcp_request_rejects_missing_header() {
        let h = HeaderMap::new();
        let s = state_with_bearer("abc123");
        assert!(!authorize_mcp_request(&h, &s));
    }

    #[test]
    fn authorize_mcp_request_rejects_missing_bearer_prefix() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("abc123"),
        );
        let s = state_with_bearer("abc123");
        assert!(!authorize_mcp_request(&h, &s));
    }

    #[test]
    fn authorize_mcp_request_rejects_wrong_token() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer wrong"),
        );
        let s = state_with_bearer("abc123");
        assert!(!authorize_mcp_request(&h, &s));
    }

    #[test]
    fn authorize_mcp_request_accepts_oauth_token() {
        use crate::oauth_server::{AuthorizeRequest as AR, TokenRequest as TR};
        use base64::Engine;
        use sha2::{Digest, Sha256};

        let oauth = Arc::new(OAuthServer::new(
            url::Url::parse("https://example.invalid/").unwrap(),
        ));

        // Drive the OAuth dance manually to get a live access token.
        let verifier: String = (0..64).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        let code = oauth
            .start_authorize(AR {
                response_type: "code".into(),
                client_id: "c".into(),
                redirect_uri: crate::oauth_server::CLAUDE_AI_REDIRECT_URI.into(),
                code_challenge: challenge,
                code_challenge_method: "S256".into(),
                state: None,
                scope: None,
            })
            .unwrap();
        let tok = oauth
            .exchange_code(TR {
                grant_type: "authorization_code".into(),
                code,
                redirect_uri: crate::oauth_server::CLAUDE_AI_REDIRECT_URI.into(),
                client_id: "c".into(),
                code_verifier: verifier,
            })
            .unwrap();

        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(&format!("Bearer {}", tok.access_token)).unwrap(),
        );
        let s = AppState {
            server: Arc::new(dummy_server()),
            bearer: None,
            oauth: Some(oauth),
        };
        assert!(authorize_mcp_request(&h, &s));
    }
}
