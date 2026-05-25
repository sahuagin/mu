//! OpenAI Codex OAuth login flow.
//!
//! Standard OAuth 2.0 with PKCE plus codex-specific query parameters
//! (`codex_cli_simplified_flow=true`, `originator=mu`). See spec
//! mu-018.
//!
//! Constants pulled from pi's auth.rs (verified working). The
//! redirect URI port (1455) is registered on OpenAI's side as part
//! of the Codex CLI app config; we can't change it.

use std::collections::HashMap;
use std::time::Duration;

use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

use super::{AuthError, OAuthToken};

// ============================================================================
// Constants (from pi's auth.rs — verified working with OpenAI)
// ============================================================================

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_PORT: u16 = 1455;
const SCOPES: &[&str] = &["openid", "profile", "email", "offline_access"];

/// How long to wait for the user to complete the browser flow
/// before giving up.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(600);

/// `originator` query param value. OpenAI accepts free strings; pi
/// uses "pi". We identify as "mu".
const ORIGINATOR: &str = "mu";

// ============================================================================
// Public API
// ============================================================================

/// Run the full OpenAI Codex OAuth flow. Opens a browser, listens
/// for the callback on `localhost:1455`, exchanges the code for
/// tokens, returns the bundle.
pub async fn login_flow() -> Result<OAuthToken, AuthError> {
    let client = build_client()?;

    // PKCE challenge for the auth-code flow.
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    // CSRF token (separate from PKCE verifier — see INV-5 in the
    // spec). The oauth2 crate generates one for us.
    let mut auth_request = client.authorize_url(CsrfToken::new_random);
    for scope in SCOPES {
        auth_request = auth_request.add_scope(Scope::new((*scope).into()));
    }
    auth_request = auth_request
        .set_pkce_challenge(pkce_challenge)
        .add_extra_param("codex_cli_simplified_flow", "true")
        .add_extra_param("originator", ORIGINATOR)
        .add_extra_param("id_token_add_organizations", "true");

    let (auth_url, csrf_token) = auth_request.url();

    // Start the callback listener BEFORE opening the browser. If the
    // user is fast, the browser-side callback would otherwise race
    // with our listener bind.
    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .map_err(|e| {
            AuthError::OAuthFlow(format!(
                "could not bind callback listener on port {CALLBACK_PORT}: {e}. \
                 Make sure no other process is using that port and retry."
            ))
        })?;

    // Always print the URL prominently up front. `webbrowser::open`
    // returns Ok as soon as it *spawns* a launcher process — even if
    // that launcher then errors out (the common Linux case is Firefox
    // showing "profile already in use" when the spawned `firefox`
    // binary can't attach to a running instance). We can't detect
    // that failure post-hoc, so we make sure the URL is on the
    // terminal for paste before we ever attempt the auto-open.
    //
    // Set `MU_NO_BROWSER=1` to skip the auto-open entirely if your
    // browser launcher misbehaves (Firefox profile collision, etc.).
    let url_str = auth_url.to_string();
    eprintln!();
    eprintln!("Open this URL to authorize mu:");
    eprintln!();
    eprintln!("    {url_str}");
    eprintln!();
    let no_browser = std::env::var("MU_NO_BROWSER")
        .map(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(false);
    if no_browser {
        eprintln!("MU_NO_BROWSER set — skipping auto-open. Waiting for callback…");
    } else {
        match webbrowser::open(&url_str) {
            Ok(_) => eprintln!(
                "Attempted browser auto-open. If your browser shows \
                 \"profile already in use\" or never loads the page, \
                 paste the URL above into a running browser tab. \
                 Waiting for callback…"
            ),
            Err(_) => eprintln!(
                "No browser launcher available — paste the URL above \
                 into a browser. Waiting for callback…"
            ),
        }
    }

    // Wait for the callback, with a generous timeout.
    let (received_code, received_state) =
        match timeout(CALLBACK_TIMEOUT, wait_for_callback(listener)).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(AuthError::CallbackTimeout),
        };

    // CSRF check.
    if received_state != *csrf_token.secret() {
        return Err(AuthError::StateMismatch);
    }

    // Exchange the auth code for tokens.
    let http_client = build_http_client()?;
    let token_result = client
        .exchange_code(AuthorizationCode::new(received_code))
        .set_pkce_verifier(pkce_verifier)
        .request_async(&http_client)
        .await
        .map_err(|e| AuthError::OAuthFlow(format!("token exchange failed: {e}")))?;

    Ok(from_token_response(&token_result))
}

/// Refresh an access token using a stored refresh token. Returns
/// the new bundle. Refresh tokens may also rotate; persist whatever
/// comes back.
pub async fn refresh_access_token(refresh_token: &str) -> Result<OAuthToken, AuthError> {
    use oauth2::RefreshToken;
    let client = build_client()?;
    let http_client = build_http_client()?;
    let token_result = client
        .exchange_refresh_token(&RefreshToken::new(refresh_token.into()))
        .request_async(&http_client)
        .await
        .map_err(|e| AuthError::OAuthFlow(format!("refresh failed: {e}")))?;
    Ok(from_token_response(&token_result))
}

// ============================================================================
// Helpers
// ============================================================================

/// Public-client (PKCE-only) oauth2 client with AuthUrl and TokenUrl set.
/// Device-auth / introspection / revocation endpoints unused — left
/// at `EndpointNotSet`.
type CodexOAuthClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

fn build_client() -> Result<CodexOAuthClient, AuthError> {
    let auth_url = AuthUrl::new(AUTHORIZE_URL.to_string())
        .map_err(|e| AuthError::OAuthFlow(format!("bad authorize url: {e}")))?;
    let token_url = TokenUrl::new(TOKEN_URL.to_string())
        .map_err(|e| AuthError::OAuthFlow(format!("bad token url: {e}")))?;
    let redirect_url = RedirectUrl::new(REDIRECT_URI.to_string())
        .map_err(|e| AuthError::OAuthFlow(format!("bad redirect url: {e}")))?;
    // oauth2 5.x: typestate builder. No client secret — public client w/ PKCE.
    let client = BasicClient::new(ClientId::new(CLIENT_ID.into()))
        .set_auth_uri(auth_url)
        .set_token_uri(token_url)
        .set_redirect_uri(redirect_url);
    Ok(client)
}

/// Build the reqwest client passed to oauth2's `request_async`.
/// `redirect::Policy::none()` is recommended by oauth2 to keep token
/// exchanges single-hop.
fn build_http_client() -> Result<reqwest::Client, AuthError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| AuthError::OAuthFlow(format!("http client build: {e}")))
}

fn from_token_response(
    resp: &oauth2::StandardTokenResponse<
        oauth2::EmptyExtraTokenFields,
        oauth2::basic::BasicTokenType,
    >,
) -> OAuthToken {
    let expires_at = resp.expires_in().map(|d| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|now| now.as_secs() + d.as_secs())
            .unwrap_or(0)
    });
    OAuthToken {
        access_token: resp.access_token().secret().clone(),
        refresh_token: resp.refresh_token().map(|t| t.secret().clone()),
        id_token: None, // oauth2 crate doesn't expose id_token in this struct;
        // future enhancement: parse from raw response if codex returns one.
        token_type: format!("{:?}", resp.token_type())
            .to_lowercase()
            .replace('"', ""),
        expires_at,
    }
}

/// Listen for the OAuth redirect, return (code, state) on success.
/// Accepts a single connection, writes back a small HTML success
/// page so the user knows they can close the tab.
async fn wait_for_callback(listener: TcpListener) -> Result<(String, String), AuthError> {
    let (mut stream, _) = listener.accept().await?;

    // Read the HTTP request. We only need the first line ("GET
    // /auth/callback?code=...&state=... HTTP/1.1") to extract the
    // query string. Read up to 4KB which is plenty for the request
    // line and headers.
    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await?;
    let raw = String::from_utf8_lossy(&buf[..n]).to_string();
    let request_line = raw.lines().next().unwrap_or("");

    // Parse out `?code=...&state=...` from the GET line.
    let params = extract_query_params(request_line);
    let code = params
        .get("code")
        .cloned()
        .ok_or_else(|| AuthError::OAuthFlow("callback missing `code` param".into()))?;
    let state = params
        .get("state")
        .cloned()
        .ok_or_else(|| AuthError::OAuthFlow("callback missing `state` param".into()))?;

    // Respond with a friendly HTML page.
    let body = "<!doctype html>\
        <html><head><title>mu — signed in</title></head>\
        <body style=\"font-family: system-ui; max-width: 40em; margin: 4em auto; padding: 2em;\">\
        <h1>Signed in.</h1>\
        <p>You can close this tab.</p>\
        </body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;

    Ok((code, state))
}

/// Pull `?code=...&state=...` out of an HTTP request line like
/// `GET /auth/callback?code=abc&state=xyz HTTP/1.1`.
fn extract_query_params(request_line: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    // Find the URL portion.
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return params;
    }
    let url = parts[1];
    let qs_start = match url.find('?') {
        Some(i) => i + 1,
        None => return params,
    };
    let qs = &url[qs_start..];
    for pair in qs.split('&') {
        let mut split = pair.splitn(2, '=');
        let key = split.next().unwrap_or("");
        let value = split.next().unwrap_or("");
        if !key.is_empty() {
            params.insert(
                urlencoding::decode(key)
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| key.into()),
                urlencoding::decode(value)
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| value.into()),
            );
        }
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b6_pkce_challenge_round_trip() {
        // The oauth2 crate handles the PKCE math; we just verify
        // the round-trip produces what we expect (challenge derives
        // from verifier with SHA256+b64url).
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let verifier_str = verifier.secret();
        assert!(verifier_str.len() >= 43 && verifier_str.len() <= 128);
        // Method should be S256.
        assert_eq!(challenge.method().as_str(), "S256");
        // Challenge is non-empty.
        assert!(!challenge.as_str().is_empty());
    }

    #[test]
    fn b7_authorize_url_has_required_params() {
        let client = build_client().expect("build client");
        let (pkce_challenge, _verifier) = PkceCodeChallenge::new_random_sha256();
        let mut req = client.authorize_url(CsrfToken::new_random);
        for scope in SCOPES {
            req = req.add_scope(Scope::new((*scope).into()));
        }
        req = req
            .set_pkce_challenge(pkce_challenge)
            .add_extra_param("codex_cli_simplified_flow", "true")
            .add_extra_param("originator", ORIGINATOR)
            .add_extra_param("id_token_add_organizations", "true");
        let (url, _csrf) = req.url();
        let url_str = url.to_string();
        assert!(url_str.contains("codex_cli_simplified_flow=true"));
        assert!(url_str.contains("originator=mu"));
        assert!(url_str.contains("id_token_add_organizations=true"));
        assert!(url_str.contains("code_challenge_method=S256"));
        assert!(url_str.contains(&urlencoding::encode(REDIRECT_URI).into_owned()));
        // Scopes
        assert!(url_str.contains("openid"));
        assert!(url_str.contains("offline_access"));
    }

    #[test]
    fn b8_extract_query_params_basic() {
        let line = "GET /auth/callback?code=abc&state=xyz HTTP/1.1";
        let p = extract_query_params(line);
        assert_eq!(p.get("code"), Some(&"abc".to_string()));
        assert_eq!(p.get("state"), Some(&"xyz".to_string()));
    }

    #[test]
    fn b8_extract_query_params_urlencoded() {
        let line = "GET /auth/callback?code=a%2Bb&state=hello%20world HTTP/1.1";
        let p = extract_query_params(line);
        assert_eq!(p.get("code"), Some(&"a+b".to_string()));
        assert_eq!(p.get("state"), Some(&"hello world".to_string()));
    }

    #[test]
    fn b8_extract_query_params_no_query() {
        let line = "GET /auth/callback HTTP/1.1";
        let p = extract_query_params(line);
        assert!(p.is_empty());
    }

    #[tokio::test]
    async fn b9_callback_listener_picks_up_code_and_state() {
        // Bind to an arbitrary free port (not 1455 — that's the
        // production port and might be in use).
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Spawn the listener.
        let server = tokio::spawn(wait_for_callback(listener));

        // Simulate the browser making the redirect call.
        let req = "GET /auth/callback?code=test_code&state=expected_state HTTP/1.1\r\n\
             Host: localhost\r\nConnection: close\r\n\r\n"
            .to_string();
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        sock.write_all(req.as_bytes()).await.unwrap();
        // Read response (drain).
        let mut buf = vec![0u8; 1024];
        let _ = sock.read(&mut buf).await;

        let (code, state) = server.await.unwrap().unwrap();
        assert_eq!(code, "test_code");
        assert_eq!(state, "expected_state");
    }
}
