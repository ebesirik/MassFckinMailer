//! Hand-rolled OAuth2 authorization-code flow with PKCE for the Gmail and
//! Outlook providers.
//!
//! We don't ship any client secret: the user registers their own OAuth app and
//! pastes its client id (and, for Google desktop clients, the client secret,
//! which is non-confidential for installed apps). The flow:
//!   1. bind a loopback listener on 127.0.0.1:<ephemeral>,
//!   2. open the browser to the authorization URL (PKCE S256 challenge),
//!   3. capture the redirect with the auth code,
//!   4. exchange code + verifier for access + refresh tokens.
//!
//! Tokens live in the OS keychain (as JSON); access tokens are refreshed on
//! demand and written back.

use crate::account::{Account, AccountConfig};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Per-provider OAuth endpoints and scope.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub authorize: String,
    pub token: String,
    pub scope: String,
    /// Redirect host label the provider expects (`127.0.0.1` for Google,
    /// `localhost` for Azure). We always bind loopback regardless.
    pub redirect_host: &'static str,
    /// Extra authorization params (e.g. Google's `access_type=offline`).
    pub extra_auth: Vec<(&'static str, String)>,
}

pub fn google_endpoints() -> Endpoints {
    Endpoints {
        authorize: "https://accounts.google.com/o/oauth2/v2/auth".into(),
        token: "https://oauth2.googleapis.com/token".into(),
        scope: "https://www.googleapis.com/auth/gmail.send".into(),
        redirect_host: "127.0.0.1",
        extra_auth: vec![
            ("access_type", "offline".into()),
            ("prompt", "consent".into()),
        ],
    }
}

pub fn microsoft_endpoints(tenant: &str) -> Endpoints {
    let tenant = if tenant.trim().is_empty() {
        "common"
    } else {
        tenant.trim()
    };
    Endpoints {
        authorize: format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize"),
        token: format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"),
        // offline_access is required to receive a refresh token.
        scope: "https://graph.microsoft.com/Mail.Send offline_access openid email".into(),
        redirect_host: "localhost",
        extra_auth: vec![("prompt", "consent".into())],
    }
}

/// Persist an OAuth account's tokens (JSON) to the keychain under its id.
pub fn store_tokens(account_id: &str, tokens: &Tokens) -> Result<(), String> {
    crate::secrets::set(account_id, &tokens.to_json()).map_err(|e| e.to_string())
}

/// Endpoints + client id derived from an OAuth account's config.
pub fn endpoints_for(account: &Account) -> Option<(Endpoints, String)> {
    match &account.config {
        AccountConfig::Gmail(c) => Some((google_endpoints(), c.client_id.clone())),
        AccountConfig::Outlook(c) => Some((microsoft_endpoints(&c.tenant), c.client_id.clone())),
        _ => None,
    }
}

/// Token set persisted in the keychain (as JSON) for an OAuth account. Holds the
/// client secret too, so refresh works without re-reading config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix ms after which the access token should be considered expired.
    pub expires_at_ms: u64,
    #[serde(default)]
    pub client_secret: String,
}

impl Tokens {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(s: &str) -> Result<Self, String> {
        serde_json::from_str(s).map_err(|e| format!("stored OAuth tokens are invalid: {e}"))
    }

    pub fn is_expired(&self) -> bool {
        now_ms() >= self.expires_at_ms
    }
}

/// The raw token endpoint response.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// (verifier, S256 challenge) PKCE pair.
pub fn pkce_pair() -> (String, String) {
    let mut bytes = [0u8; 64];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

pub fn random_state() -> String {
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Build the authorization URL the user's browser is sent to.
pub fn build_auth_url(
    ep: &Endpoints,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> String {
    let mut url = url::Url::parse(&ep.authorize).expect("valid authorize URL");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", client_id);
        q.append_pair("response_type", "code");
        q.append_pair("redirect_uri", redirect_uri);
        q.append_pair("scope", &ep.scope);
        q.append_pair("code_challenge", challenge);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("state", state);
        for (k, v) in &ep.extra_auth {
            q.append_pair(k, v);
        }
    }
    url.into()
}

/// Parse a URL query string into a map (percent-decoded).
pub fn parse_query(query: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

/// Run the full interactive authorization for `account`, returning fresh tokens
/// (with `client_secret` folded in for later refreshes). Opens the browser and
/// waits up to 5 minutes for the redirect.
pub async fn connect(account: &Account, client_secret: &str) -> Result<Tokens, String> {
    let (ep, client_id) =
        endpoints_for(account).ok_or_else(|| "not an OAuth account".to_string())?;
    if client_id.trim().is_empty() {
        return Err("a client ID is required".into());
    }

    let (verifier, challenge) = pkce_pair();
    let state = random_state();

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("could not start local listener: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect_uri = format!("http://{}:{}", ep.redirect_host, port);

    let auth_url = build_auth_url(&ep, &client_id, &redirect_uri, &challenge, &state);
    open::that(&auth_url).map_err(|e| format!("could not open the browser: {e}"))?;

    let code = tokio::time::timeout(Duration::from_secs(300), accept_code(&listener, &state))
        .await
        .map_err(|_| "timed out waiting for authorization (5 min)".to_string())??;

    let http = reqwest::Client::new();
    let mut tokens = exchange_code(
        &http,
        &ep,
        &client_id,
        client_secret,
        &code,
        &verifier,
        &redirect_uri,
    )
    .await?;
    tokens.client_secret = client_secret.to_string();
    Ok(tokens)
}

/// Accept loopback connections until the OAuth redirect arrives, returning the
/// authorization code. Responds to the browser with a small confirmation page.
async fn accept_code(listener: &TcpListener, expected_state: &str) -> Result<String, String> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| format!("listener error: {e}"))?;

        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);

        let body = "<!doctype html><html><body style=\"font-family:sans-serif;padding:2rem\">\
            <h3>Authorization complete</h3><p>You can close this tab and return to MassFckinMailer.</p>\
            </body></html>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;

        if let Some(error) = params.get("error") {
            let detail = params
                .get("error_description")
                .map(|d| format!("{error}: {d}"))
                .unwrap_or_else(|| error.clone());
            return Err(format!("authorization denied: {detail}"));
        }
        match (params.get("code"), params.get("state")) {
            (Some(_), Some(state)) if state != expected_state => {
                return Err("state mismatch — possible CSRF, aborting".into());
            }
            (Some(code), Some(_)) => return Ok(code.clone()),
            // Ignore unrelated requests (e.g. the browser's favicon probe).
            _ => continue,
        }
    }
}

async fn exchange_code(
    http: &reqwest::Client,
    ep: &Endpoints,
    client_id: &str,
    client_secret: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Tokens, String> {
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("client_id", client_id.to_string()),
        ("code_verifier", verifier.to_string()),
    ];
    if !client_secret.is_empty() {
        form.push(("client_secret", client_secret.to_string()));
    }

    let response: TokenResponse = post_token(http, &ep.token, &form).await?;
    let access_token = response
        .access_token
        .ok_or("token endpoint returned no access token")?;
    let refresh_token = response
        .refresh_token
        .ok_or("token endpoint returned no refresh token (ensure offline access / consent)")?;
    Ok(Tokens {
        access_token,
        refresh_token,
        expires_at_ms: expiry_from(response.expires_in),
        client_secret: client_secret.to_string(),
    })
}

/// Exchange the refresh token for a new access token, updating `tokens` in place
/// (some providers also rotate the refresh token).
pub async fn refresh(
    http: &reqwest::Client,
    ep: &Endpoints,
    client_id: &str,
    tokens: &mut Tokens,
) -> Result<(), String> {
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", tokens.refresh_token.clone()),
        ("client_id", client_id.to_string()),
        ("scope", ep.scope.clone()),
    ];
    if !tokens.client_secret.is_empty() {
        form.push(("client_secret", tokens.client_secret.clone()));
    }

    let response: TokenResponse = post_token(http, &ep.token, &form).await?;
    tokens.access_token = response
        .access_token
        .ok_or("refresh returned no access token")?;
    tokens.expires_at_ms = expiry_from(response.expires_in);
    if let Some(rt) = response.refresh_token {
        tokens.refresh_token = rt;
    }
    Ok(())
}

async fn post_token(
    http: &reqwest::Client,
    token_url: &str,
    form: &[(&str, String)],
) -> Result<TokenResponse, String> {
    let resp = http
        .post(token_url)
        .form(form)
        .send()
        .await
        .map_err(|e| format!("token request failed: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let parsed: TokenResponse = serde_json::from_str(&body)
        .map_err(|_| format!("unexpected token response ({status}): {body}"))?;
    if let Some(error) = &parsed.error {
        let detail = parsed
            .error_description
            .clone()
            .unwrap_or_else(|| error.clone());
        return Err(format!("OAuth error: {detail}"));
    }
    Ok(parsed)
}

/// Compute an expiry instant with a 60s safety margin.
fn expiry_from(expires_in: Option<u64>) -> u64 {
    let secs = expires_in.unwrap_or(3600).saturating_sub(60);
    now_ms() + secs * 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let (verifier, challenge) = pkce_pair();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected);
        // Verifier length must be within the 43..=128 char PKCE range.
        assert!((43..=128).contains(&verifier.len()));
    }

    #[test]
    fn auth_url_has_pkce_and_scope() {
        let ep = google_endpoints();
        let url = build_auth_url(&ep, "cid.apps", "http://127.0.0.1:5000", "chal", "st8");
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("client_id=cid.apps"));
        assert!(url.contains("access_type=offline"));
        // redirect_uri is percent-encoded.
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5000"));
        assert!(url.contains("gmail.send"));
    }

    #[test]
    fn parses_redirect_query() {
        let params = parse_query("code=4%2F0Ab&state=xyz&scope=a%20b");
        assert_eq!(params.get("code").unwrap(), "4/0Ab");
        assert_eq!(params.get("state").unwrap(), "xyz");
        assert_eq!(params.get("scope").unwrap(), "a b");
    }

    #[test]
    fn microsoft_endpoints_default_tenant() {
        let ep = microsoft_endpoints("");
        assert!(ep.authorize.contains("/common/"));
        assert!(ep.scope.contains("offline_access"));
    }

    #[test]
    fn tokens_json_round_trip_and_expiry() {
        let t = Tokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at_ms: 0,
            client_secret: "s".into(),
        };
        let restored = Tokens::from_json(&t.to_json()).unwrap();
        assert_eq!(restored.refresh_token, "r");
        assert!(restored.is_expired()); // expires_at_ms = 0 is in the past
    }
}
