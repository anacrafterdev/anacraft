//! The hosted MCP connector's front door for assistants that sign in rather
//! than being handed a link.
//!
//! A connector link carries its token in the URL, which is what most clients
//! have room for. A client that follows the MCP authorization spec wants
//! something else: the bare URL, a 401 that says where to sign in, and an
//! OAuth 2.1 server it can register with, send the user through and trade a
//! code at. That server is this module:
//!
//! - `/.well-known/oauth-protected-resource` names `/mcp` and this origin
//!   as the server that issues tokens for it (RFC 9728);
//! - `/.well-known/oauth-authorization-server` says where everything is
//!   (RFC 8414);
//! - `/oauth/register` hands any client an id (RFC 7591);
//! - `/oauth/authorize` is the consent page, in `views.rs`, behind the same
//!   Google sign-in the pages use;
//! - `/oauth/token` trades a code, with its PKCE verifier, for tokens.
//!
//! Nothing here is written down. A client id is its registration, sealed; an
//! access token is the account and an expiry, sealed; a refresh token is the
//! account and the client, sealed — each under a key of its own derived from
//! the grant key ([`Grants::seal_for`]). The one thing held in memory is the
//! code, for the minute between consent and the trade, so it can be spent
//! once. What turns a user's tokens off is what turns their links off:
//! forgetting the grant, which the connector page does.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::grants::Grants;
use super::hosted::Hosted;
use super::{now, App};
use crate::auth::Account;

/// How long an access token reads for before the client refreshes it.
const ACCESS_FOR: u64 = 60 * 60;

/// How long a code waits between the consent page and the token endpoint.
const CODE_FOR: Duration = Duration::from_secs(2 * 60);

/// Registration limits, so a client id — which is the registration, sealed —
/// stays a size a URL can carry.
const MAX_URIS: usize = 8;
const MAX_URI: usize = 512;
const MAX_NAME: usize = 80;

/// The routes an MCP client reaches without a browser: discovery, register,
/// token. The consent page is a view, and lives with the views.
pub(super) fn router() -> Router<Arc<App>> {
    Router::new()
        .route("/.well-known/oauth-protected-resource", get(protected))
        .route("/.well-known/oauth-protected-resource/mcp", get(protected))
        // The older spelling of the endpoint, which the first connector links
        // named: its metadata names it back, since a client checks that the
        // resource it is told about is the one it is talking to.
        .route(
            "/.well-known/oauth-protected-resource/v1/mcp",
            get(protected_v1),
        )
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route("/oauth/register", post(register))
        .route("/oauth/token", post(token))
}

/// The paths a POST may reach from no origin, or another one: callers of
/// these are assistants' servers and native apps, and what they answer to is
/// what is in the request, never a cookie a forged form could ride on.
pub(super) fn cross_origin(path: &str) -> bool {
    matches!(path, "/oauth/register" | "/oauth/token")
}

/// The connector's endpoint, as a resource indicator (RFC 8707).
fn resource(public: &str) -> String {
    format!("{public}{MCP}")
}

/// Where the hosted connector answers. `/v1/mcp` does too, for the links
/// handed out before it had an address of its own.
pub(super) const MCP: &str = "/mcp";

/// The `WWW-Authenticate` a 401 from the connector wears, which is how a
/// client that was given nothing but the URL finds its way here — to the
/// metadata for the path it asked on (RFC 9728 §3.1).
pub(super) fn challenge(public: &str, path: &str, error: Option<&str>) -> HeaderValue {
    let mut value =
        format!("Bearer resource_metadata=\"{public}/.well-known/oauth-protected-resource{path}\"");
    if let Some(error) = error {
        value.push_str(&format!(", error=\"{error}\""));
    }
    HeaderValue::from_str(&value).expect("an https origin is ASCII")
}

/// The grant store, when the connector is on — without it there is nothing a
/// token could read with, so there is nothing to sign into either.
fn store(app: &App) -> Option<(&Hosted, &Grants)> {
    let hosted = app.hosted.as_ref().expect("hosted route");
    hosted.grants.as_ref().map(|grants| (hosted, grants))
}

fn off() -> Response {
    (
        StatusCode::NOT_FOUND,
        "this server has no MCP connector switched on",
    )
        .into_response()
}

// ---------------------------------------------------------------- discovery ---

async fn protected(State(app): State<Arc<App>>) -> Response {
    described(&app, MCP)
}

async fn protected_v1(State(app): State<Arc<App>>) -> Response {
    described(&app, "/v1/mcp")
}

fn described(app: &App, path: &str) -> Response {
    let Some((hosted, _)) = store(app) else {
        return off();
    };
    Json(json!({
        "resource": format!("{}{path}", hosted.public),
        "authorization_servers": [hosted.public],
        "bearer_methods_supported": ["header"],
        "resource_name": "anacraft — Google Analytics 4",
        "resource_documentation": "https://anacraft.dev/mcp.html",
    }))
    .into_response()
}

async fn metadata(State(app): State<Arc<App>>) -> Response {
    let Some((hosted, _)) = store(&app) else {
        return off();
    };
    let public = &hosted.public;
    Json(json!({
        "issuer": public,
        "authorization_endpoint": format!("{public}/oauth/authorize"),
        "token_endpoint": format!("{public}/oauth/token"),
        "registration_endpoint": format!("{public}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "authorization_response_iss_parameter_supported": true,
        "service_documentation": "https://anacraft.dev/mcp.html",
    }))
    .into_response()
}

// ------------------------------------------------------------- registration ---

/// A registered client: what the consent page names and where a code may go.
/// Sealed, this is the client id.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Client {
    name: String,
    uris: Vec<String>,
}

#[derive(Deserialize)]
struct Registration {
    #[serde(default)]
    redirect_uris: Vec<String>,
    #[serde(default)]
    client_name: Option<String>,
}

/// An OAuth error as RFC 6749 spells it.
fn refuse(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        Json(json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

async fn register(State(app): State<Arc<App>>, body: axum::body::Bytes) -> Response {
    let Some((_, grants)) = store(&app) else {
        return off();
    };
    let Ok(asked) = serde_json::from_slice::<Registration>(&body) else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "the registration is not JSON this server reads",
        );
    };
    let client = match client_of(asked) {
        Ok(client) => client,
        Err(why) => return refuse(StatusCode::BAD_REQUEST, "invalid_redirect_uri", &why),
    };
    let id = grants.seal_for(
        "client",
        &serde_json::to_vec(&client).expect("a client serializes"),
    );
    (
        StatusCode::CREATED,
        Json(json!({
            "client_id": id,
            "client_id_issued_at": now(),
            "client_name": client.name,
            "redirect_uris": client.uris,
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            // Whatever was asked for: a client id anybody can register is
            // not a secret, so there is no secret to hand out with it. PKCE
            // is what proves the caller at the token endpoint.
            "token_endpoint_auth_method": "none",
        })),
    )
        .into_response()
}

fn client_of(asked: Registration) -> Result<Client, String> {
    if asked.redirect_uris.is_empty() {
        return Err("a client registers at least one redirect_uri".into());
    }
    if asked.redirect_uris.len() > MAX_URIS {
        return Err(format!("at most {MAX_URIS} redirect_uris"));
    }
    for uri in &asked.redirect_uris {
        if !redirectable(uri) {
            return Err(format!(
                "{uri} is not a redirect this server sends codes to — https, http on \
                 loopback, or an app's own scheme"
            ));
        }
    }
    let name: String = asked
        .client_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("An MCP client")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect();
    Ok(Client {
        name,
        uris: asked.redirect_uris,
    })
}

/// Where a code may be sent: https anywhere; http only back to this device,
/// for a CLI's loopback listener (RFC 8252 §7.3); or an app's own scheme,
/// as a desktop client like Cursor registers (§7.1). Never a scheme a
/// browser would run or read instead of hand to an app, and never with a
/// fragment, which a code must not ride in.
fn redirectable(uri: &str) -> bool {
    if uri.len() > MAX_URI || uri.contains('#') || uri.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let Some((scheme, rest)) = uri.split_once(':') else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    match scheme.as_str() {
        "https" => host_of(rest).is_some_and(|host| !host.is_empty()),
        "http" => host_of(rest)
            .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]")),
        "javascript" | "data" | "file" | "vbscript" | "about" | "blob" | "filesystem" | "ws"
        | "wss" => false,
        _ => {
            !scheme.is_empty()
                && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
                && !rest.is_empty()
        }
    }
}

/// The host of `//host[:port]/path`, lowercased, with no userinfo allowed —
/// `https://claude.ai@evil.example` names evil.example, and a consent page
/// that showed the first half would be lying.
fn host_of(rest: &str) -> Option<String> {
    let authority = rest.strip_prefix("//")?;
    let authority = authority.split(['/', '?']).next().unwrap_or_default();
    if authority.contains('@') {
        return None;
    }
    let host = if authority.starts_with('[') {
        &authority[..=authority.find(']')?]
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    Some(host.to_ascii_lowercase())
}

// ---------------------------------------------------------------- consent ---

/// `/oauth/authorize`'s query, and the consent form's hidden fields: the
/// same request, carried through the page untouched.
#[derive(Deserialize, Serialize, Clone, Default)]
pub(super) struct Ask {
    #[serde(default)]
    pub response_type: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub redirect_uri: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub code_challenge: String,
    #[serde(default)]
    pub code_challenge_method: String,
    #[serde(default)]
    pub resource: String,
    #[serde(default)]
    pub scope: String,
}

/// A request good enough to put in front of the user.
pub(super) struct Asked {
    /// What the client called itself — its word, not ours.
    pub name: String,
    /// Where the answer goes, which is what the page is honest about.
    pub to: String,
    redirect_uri: String,
}

/// Why a request never reached the page.
pub(super) enum Unasked {
    /// Nothing trustworthy to send an error back to: said on a page of ours.
    Here(String),
    /// A client we know, at a redirect it registered: told there.
    There(Box<Response>),
}

/// Check an authorization request, in the order RFC 6749 §4.1.2.1 wants:
/// the client and the redirect first, since until both are known good an
/// error has nowhere safe to go.
pub(super) fn check(app: &App, ask: &Ask) -> Result<Asked, Unasked> {
    let hosted = app.hosted.as_ref().expect("hosted route");
    let grants = hosted
        .grants
        .as_ref()
        .ok_or_else(|| Unasked::Here("this server has no MCP connector switched on.".into()))?;
    let client = grants
        .open_for("client", &ask.client_id)
        .and_then(|raw| serde_json::from_slice::<Client>(&raw).ok())
        .ok_or_else(|| {
            Unasked::Here(
                "the app that sent you here is not registered with anacraft — go back to it \
                 and connect again."
                    .into(),
            )
        })?;
    let redirect_uri = match (ask.redirect_uri.as_str(), client.uris.as_slice()) {
        ("", [only]) => only.clone(),
        (given, uris) if uris.iter().any(|uri| uri == given) => given.to_string(),
        _ => {
            return Err(Unasked::Here(
                "the app that sent you here asked for its answer at an address it never \
                 registered, so none was sent."
                    .into(),
            ))
        }
    };

    let there = |error: &str| {
        Unasked::There(Box::new(back(
            hosted,
            &redirect_uri,
            &ask.state,
            Err(error),
        )))
    };
    if ask.response_type != "code" {
        return Err(there("unsupported_response_type"));
    }
    if ask.code_challenge.is_empty() || ask.code_challenge_method != "S256" {
        return Err(there("invalid_request"));
    }
    if !ask.resource.is_empty() && !same_resource(&ask.resource, &resource(&hosted.public)) {
        return Err(there("invalid_target"));
    }
    let to = match redirect_uri.split_once(':') {
        Some((_, rest)) => host_of(rest).unwrap_or_else(|| redirect_uri.clone()),
        None => redirect_uri.clone(),
    };
    Ok(Asked {
        name: client.name,
        to,
        redirect_uri,
    })
}

/// The resource a client names: the connector, at either of its paths, or
/// this origin — some clients send the one, some the other.
fn same_resource(asked: &str, ours: &str) -> bool {
    let asked = asked.trim_end_matches('/');
    let Some(origin) = ours.strip_suffix(MCP) else {
        return asked == ours;
    };
    asked == ours || asked == origin || asked == format!("{origin}/v1/mcp")
}

/// The consent page's yes: a code, one use, two minutes, sent back to the
/// client.
pub(super) fn allow(app: &App, account: &Account, ask: &Ask, asked: &Asked) -> Response {
    let hosted = app.hosted.as_ref().expect("hosted route");
    let code = crate::auth::nonce(43);
    hosted.codes.put(
        &code,
        Code {
            sub: account.sub.clone(),
            client: digest(&ask.client_id),
            redirect_uri: asked.redirect_uri.clone(),
            challenge: ask.code_challenge.clone(),
            born: Instant::now(),
        },
    );
    back(hosted, &asked.redirect_uri, &ask.state, Ok(&code))
}

/// The consent page's no.
pub(super) fn deny(app: &App, ask: &Ask, asked: &Asked) -> Response {
    let hosted = app.hosted.as_ref().expect("hosted route");
    back(
        hosted,
        &asked.redirect_uri,
        &ask.state,
        Err("access_denied"),
    )
}

/// Off to the client's redirect with a code or an error, the `state` it sent,
/// and who is answering (RFC 9207), so a client talking to several servers
/// cannot be told one's code came from another.
fn back(hosted: &Hosted, redirect_uri: &str, state: &str, outcome: Result<&str, &str>) -> Response {
    let mut url = redirect_uri.to_string();
    url.push(if url.contains('?') { '&' } else { '?' });
    match outcome {
        Ok(code) => url.push_str(&format!("code={}", encode(code))),
        Err(error) => url.push_str(&format!("error={error}")),
    }
    if !state.is_empty() {
        url.push_str(&format!("&state={}", encode(state)));
    }
    url.push_str(&format!("&iss={}", encode(&hosted.public)));
    Redirect::to(&url).into_response()
}

fn encode(value: &str) -> String {
    crate::license::encode(value)
}

// ------------------------------------------------------------------ codes ---

/// A consent, waiting to be traded.
pub(super) struct Code {
    sub: String,
    client: String,
    redirect_uri: String,
    challenge: String,
    born: Instant,
}

/// The codes in flight, by the SHA-256 of the code — so what sits in memory
/// is not itself a code anybody could present.
#[derive(Default)]
pub(super) struct Codes(Mutex<HashMap<String, Code>>);

impl Codes {
    fn put(&self, code: &str, pending: Code) {
        self.0
            .lock()
            .expect("code lock poisoned")
            .insert(digest(code), pending);
    }

    /// Taken off the table whatever happens next: a code presented twice is
    /// refused the second time even if the first was refused too.
    fn take(&self, code: &str) -> Option<Code> {
        let found = self
            .0
            .lock()
            .expect("code lock poisoned")
            .remove(&digest(code))?;
        (found.born.elapsed() < CODE_FOR).then_some(found)
    }

    pub fn sweep(&self) {
        self.0
            .lock()
            .expect("code lock poisoned")
            .retain(|_, code| code.born.elapsed() < CODE_FOR);
    }
}

// ------------------------------------------------------------------ tokens ---

#[derive(Deserialize, Default)]
struct Trade {
    #[serde(default)]
    grant_type: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    code_verifier: String,
    #[serde(default)]
    refresh_token: String,
}

#[derive(Serialize, Deserialize)]
struct Bearer {
    sub: String,
    exp: u64,
}

#[derive(Serialize, Deserialize)]
struct Renewal {
    sub: String,
    client: String,
}

async fn token(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(mut trade): Form<Trade>,
) -> Response {
    let Some((_, grants)) = store(&app) else {
        return off();
    };
    // A client that sends its id the Basic way rather than in the body.
    if trade.client_id.is_empty() {
        trade.client_id = basic_client(&headers).unwrap_or_default();
    }
    if grants.open_for("client", &trade.client_id).is_none() {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "that client_id is not one this server registered",
        );
    }
    let client = digest(&trade.client_id);

    let sub = match trade.grant_type.as_str() {
        "authorization_code" => {
            let Some(code) = app
                .hosted
                .as_ref()
                .expect("hosted route")
                .codes
                .take(&trade.code)
            else {
                return invalid_grant("that code is spent, expired, or was never issued");
            };
            if code.client != client || code.redirect_uri != trade.redirect_uri {
                return invalid_grant("that code was issued to another client or redirect");
            }
            if !super::same(&challenge_of(&trade.code_verifier), &code.challenge) {
                return invalid_grant("the code_verifier does not match the code_challenge");
            }
            code.sub
        }
        "refresh_token" => {
            let Some(renewal) = grants
                .open_for("refresh", &trade.refresh_token)
                .and_then(|raw| serde_json::from_slice::<Renewal>(&raw).ok())
            else {
                return invalid_grant("that refresh_token is not one this server issued");
            };
            if renewal.client != client {
                return invalid_grant("that refresh_token was issued to another client");
            }
            renewal.sub
        }
        _ => {
            return refuse(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                "authorization_code or refresh_token",
            )
        }
    };

    // The grant is what the token reads with, and forgetting it is how a
    // user turns the connector off: no grant, no token.
    let account = Account {
        sub: sub.clone(),
        email: None,
    };
    match grants.has(&account).await {
        Ok(true) => {}
        Ok(false) => {
            return invalid_grant(
                "the connector was turned off for this account — sign in again at the app",
            )
        }
        Err(err) => {
            return refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "temporarily_unavailable",
                &err.to_string(),
            )
        }
    }

    let access = grants.seal_for(
        "access",
        &serde_json::to_vec(&Bearer {
            sub: sub.clone(),
            exp: now() + ACCESS_FOR,
        })
        .expect("a bearer serializes"),
    );
    let refresh = grants.seal_for(
        "refresh",
        &serde_json::to_vec(&Renewal { sub, client }).expect("a renewal serializes"),
    );
    Json(json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": ACCESS_FOR,
        "refresh_token": refresh,
    }))
    .into_response()
}

fn invalid_grant(why: &str) -> Response {
    refuse(StatusCode::BAD_REQUEST, "invalid_grant", why)
}

fn basic_client(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let encoded = value.strip_prefix("Basic ")?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let pair = String::from_utf8(raw).ok()?;
    Some(
        pair.split_once(':')
            .map_or(pair.as_str(), |(id, _)| id)
            .to_string(),
    )
}

fn challenge_of(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn digest(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What a presented bearer is, if it is one of this module's access tokens.
pub(super) enum Access {
    Live { sub: String },
    Expired,
}

pub(super) fn access(grants: &Grants, presented: &str) -> Option<Access> {
    let bearer: Bearer = serde_json::from_slice(&grants.open_for("access", presented)?).ok()?;
    Some(if bearer.exp <= now() {
        Access::Expired
    } else {
        Access::Live { sub: bearer.sub }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_goes_only_where_a_browser_would_hand_it_to_the_app() {
        for good in [
            "https://claude.ai/api/mcp/auth_callback",
            "https://chatgpt.com/connector_platform_oauth_redirect",
            "http://localhost:6274/oauth/callback",
            "http://127.0.0.1:33418",
            "http://[::1]:8080/cb",
            "cursor://anysphere.cursor-mcp/oauth/callback",
            "vscode://ms-vscode.mcp/callback",
        ] {
            assert!(redirectable(good), "{good} should be allowed");
        }
        for bad in [
            "http://evil.example/cb",
            "http://localhost.evil.example/cb",
            "https://claude.ai@evil.example/cb",
            "https://claude.ai/cb#frag",
            "javascript:alert(1)",
            "data:text/html,hi",
            "file:///etc/passwd",
            "https://",
            "not a uri",
            "",
        ] {
            assert!(!redirectable(bad), "{bad} should be refused");
        }
    }

    #[test]
    fn a_registration_is_bounded_and_named() {
        let client = client_of(Registration {
            redirect_uris: vec!["https://claude.ai/api/mcp/auth_callback".into()],
            client_name: Some("  Claude\u{7}  ".into()),
        })
        .unwrap();
        assert_eq!(client.name, "Claude");
        assert!(client_of(Registration {
            redirect_uris: vec![],
            client_name: None,
        })
        .is_err());
        assert!(client_of(Registration {
            redirect_uris: vec!["https://a.example/cb".into(); MAX_URIS + 1],
            client_name: None,
        })
        .is_err());
        let long = client_of(Registration {
            redirect_uris: vec!["https://a.example/cb".into()],
            client_name: Some("x".repeat(500)),
        })
        .unwrap();
        assert_eq!(long.name.len(), MAX_NAME);
    }

    #[test]
    fn the_consent_page_names_the_host_a_code_really_goes_to() {
        assert_eq!(
            host_of("//claude.ai/api/mcp/auth_callback").as_deref(),
            Some("claude.ai")
        );
        assert_eq!(host_of("//Claude.AI:443?x=1").as_deref(), Some("claude.ai"));
        assert_eq!(host_of("//[::1]:8080/cb").as_deref(), Some("[::1]"));
        assert_eq!(host_of("//claude.ai@evil.example/"), None);
    }

    #[test]
    fn a_resource_is_the_endpoint_or_its_origin() {
        let ours = resource("https://app.anacraft.dev");
        assert!(same_resource("https://app.anacraft.dev/mcp", &ours));
        assert!(same_resource("https://app.anacraft.dev/mcp/", &ours));
        assert!(same_resource("https://app.anacraft.dev/v1/mcp", &ours));
        assert!(same_resource("https://app.anacraft.dev", &ours));
        assert!(!same_resource("https://evil.example/mcp", &ours));
        assert!(!same_resource("https://app.anacraft.dev/v2/mcp", &ours));
    }

    #[test]
    fn a_code_is_spent_once() {
        let codes = Codes::default();
        let pending = || Code {
            sub: "110147".into(),
            client: "c".into(),
            redirect_uri: "https://claude.ai/cb".into(),
            challenge: challenge_of("verifier"),
            born: Instant::now(),
        };
        codes.put("abc", pending());
        assert!(codes.take("nope").is_none());
        assert_eq!(codes.take("abc").map(|c| c.sub).as_deref(), Some("110147"));
        assert!(codes.take("abc").is_none(), "a second trade is refused");

        codes.put(
            "old",
            Code {
                born: Instant::now() - CODE_FOR - Duration::from_secs(1),
                ..pending()
            },
        );
        assert!(codes.take("old").is_none(), "an old code is refused");
    }

    #[test]
    fn a_verifier_matches_its_challenge_as_rfc_7636_spells_it() {
        // Appendix B of RFC 7636.
        assert_eq!(
            challenge_of("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_challenge_points_at_the_metadata() {
        let value = challenge("https://app.anacraft.dev", MCP, Some("invalid_token"));
        assert_eq!(
            value.to_str().unwrap(),
            "Bearer resource_metadata=\"https://app.anacraft.dev/.well-known/oauth-protected-resource/mcp\", error=\"invalid_token\""
        );
        let old = challenge("https://app.anacraft.dev", "/v1/mcp", None);
        assert!(old
            .to_str()
            .unwrap()
            .ends_with("/.well-known/oauth-protected-resource/v1/mcp\""));
    }
}
