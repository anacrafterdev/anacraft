//! `craft serve --public-url`: one server, many visitors, each on a session
//! of their own.
//!
//! The local server has one person in front of it and keeps everything that
//! person owns where the CLI keeps it — `~/.anacraft`, the config file, one
//! token printed on a banner. A server on the public web has nobody's machine
//! to keep anything on, and every visitor is somebody else. So nothing here
//! touches the disk: a sign-in lives in a [`Session`], in memory, for as long
//! as its Google access token does, and the handlers reach it through a
//! [`Ctx`] rather than through the files.
//!
//! The sign-in is Google's web flow rather than the CLI's loopback one — a
//! *Web application* client, redirecting to `<public>/oauth/callback` — with
//! PKCE and a `state` bound to a cookie, and online access, so the server is
//! never handed a refresh token it would then have to keep safe.
//!
//! What the local server guards with its token and its origin check, this one
//! guards with the session cookie (`__Host-`, `Secure`, `HttpOnly`,
//! `SameSite=Lax`) and an exact-origin check on every POST, which between them
//! leave a cross-site form nothing to spend.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::grants::Grants;
use super::{now, App};
use crate::auth::{self, Account, Auth, ClientCreds, Store, Tokens, SCOPE_EDIT};
use crate::config::Config;
use crate::ga::Ga;
use crate::license::{self, Tier};

/// The session cookie. `__Host-` is the browser's own promise that it was set
/// by this host over https, with `Path=/` and no `Domain` — so no sibling
/// subdomain can plant one.
pub(super) const SESSION: &str = "__Host-craft_session";

/// The OAuth `state`, held by the browser that started the sign-in, so the
/// callback can tell its own round trip from one somebody else started.
const OAUTH: &str = "__Host-craft_oauth";

/// The palette a visitor picked. Not a secret and not a session: somebody
/// who has not signed in can still change the colours of the sign-in page.
pub(super) const PALETTE: &str = "craft_pal";

/// Idle this long and a session is gone, whatever its token says.
const IDLE: u64 = 30 * 60;

/// How long a sign-in may sit on Google's consent screen.
const PENDING: Duration = Duration::from_secs(10 * 60);

/// How long a paid answer from the subscription service is believed. Unpaid
/// is asked again every time: a page about to offer a checkout should not
/// offer one to somebody who paid ten seconds ago.
const PAID_FOR: Duration = Duration::from_secs(10 * 60);

pub(super) struct Hosted {
    /// `https://app.anacraft.dev`, with no trailing slash. Also the one
    /// origin a POST may come from.
    pub public: String,
    /// The Web client. `None` under `--demo`, which signs nobody in.
    web: Option<ClientCreds>,
    /// Keyed by the SHA-256 of the cookie, so what sits in memory is not
    /// itself a cookie anybody could present.
    sessions: Mutex<HashMap<[u8; 32], Arc<Session>>>,
    /// Sign-ins on their way through Google, by `state`.
    pending: Mutex<HashMap<String, Pending>>,
    /// Where signed-in users' grants are kept for their MCP connector.
    /// `None` leaves the connector off.
    pub grants: Option<Grants>,
    /// One MCP server per connector link in use, so an assistant's run of
    /// questions is not a Supabase lookup and a token refresh apiece.
    mcp: tokio::sync::Mutex<HashMap<String, Built>>,
}

/// An MCP server built for one connector link, and when. Rebuilt after
/// [`REBUILD`], which is how a plan bought or cancelled since reaches it.
struct Built {
    at: Instant,
    server: Arc<tokio::sync::Mutex<crate::mcp::Server>>,
}

const REBUILD: Duration = Duration::from_secs(10 * 60);

struct Pending {
    verifier: String,
    born: Instant,
}

/// One visitor, signed in.
///
/// No `Debug`, like [`Tokens`]: this is somebody's Google credential.
pub(super) struct Session {
    pub account: Account,
    web: ClientCreds,
    /// Shared with every [`Auth`] made for this session, so a token it
    /// refreshes is the one the next request finds.
    tokens: Arc<Mutex<Option<Tokens>>>,
    /// The config this visitor would have on their own machine: the
    /// properties they have picked, and which one is the default.
    cfg: Mutex<Config>,
    tier: Mutex<Option<(Instant, Option<Tier>)>>,
    last: AtomicU64,
}

impl Session {
    /// Past its idle limit, or past the access token it was signed in with
    /// and holding no refresh token to get another.
    fn expired(&self, now_secs: u64, now_utc: chrono::DateTime<chrono::Utc>) -> bool {
        let idle = now_secs.saturating_sub(self.last.load(Ordering::Relaxed)) > IDLE;
        let spent = self
            .tokens
            .lock()
            .expect("token lock poisoned")
            .as_ref()
            .map_or(true, |t| {
                t.refresh_token.is_empty() && t.expires_at <= now_utc
            });
        idle || spent
    }
}

impl Hosted {
    pub fn new(public: String, web: Option<ClientCreds>, grants: Option<Grants>) -> Hosted {
        Hosted {
            public,
            web,
            sessions: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            grants,
            mcp: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The connector link for one account and property, if the connector is on.
    pub fn connector(&self, account: &Account, property: &str) -> Option<super::Connector> {
        let grants = self.grants.as_ref()?;
        Some(super::Connector::at(
            format!("{}/v1/mcp", self.public),
            grants.connector_token(account),
            Some(property),
        ))
    }

    fn redirect_uri(&self) -> String {
        format!("{}/oauth/callback", self.public)
    }

    fn auth(&self, store: Store) -> Result<Auth> {
        let web = self
            .web
            .clone()
            .context("this server signs nobody in (--demo)")?;
        Ok(Auth::with(reqwest::Client::new(), web, store))
    }

    /// The live session a request's cookie names, touching its idle clock.
    fn find(&self, headers: &HeaderMap) -> Option<Arc<Session>> {
        let id = cookie(headers, SESSION)?;
        let key = digest(&id);
        let mut sessions = self.sessions.lock().expect("session lock poisoned");
        let session = sessions.get(&key)?.clone();
        if session.expired(now(), chrono::Utc::now()) {
            sessions.remove(&key);
            return None;
        }
        session.last.store(now(), Ordering::Relaxed);
        Some(session)
    }

    /// A fresh session for a sign-in that just came back, and the cookie
    /// value that opens it. Always a new id: an id somebody else chose and
    /// planted before the sign-in must never become a signed-in one.
    fn open(&self, account: Account, tokens: Tokens) -> Result<String> {
        let web = self
            .web
            .clone()
            .context("this server signs nobody in (--demo)")?;
        let id = auth::nonce(43);
        let session = Arc::new(Session {
            account,
            web,
            tokens: Arc::new(Mutex::new(Some(tokens))),
            cfg: Mutex::new(Config::default()),
            tier: Mutex::new(None),
            last: AtomicU64::new(now()),
        });
        self.sessions
            .lock()
            .expect("session lock poisoned")
            .insert(digest(&id), session);
        Ok(id)
    }

    fn close(&self, headers: &HeaderMap) {
        if let Some(id) = cookie(headers, SESSION) {
            self.sessions
                .lock()
                .expect("session lock poisoned")
                .remove(&digest(&id));
        }
    }

    /// Take a pending sign-in off the table: once, and only by the browser
    /// holding its cookie, and only while it is young.
    fn redeem(&self, state: &str, held: Option<&str>) -> Option<String> {
        redeem(
            &mut self.pending.lock().expect("pending lock poisoned"),
            state,
            held,
            Instant::now(),
        )
    }

    /// Drop what has expired. Run on a timer, so memory is bounded by the
    /// visitors of the last half hour rather than by every visitor ever.
    pub fn sweep(&self) {
        let (secs, utc) = (now(), chrono::Utc::now());
        self.sessions
            .lock()
            .expect("session lock poisoned")
            .retain(|_, s| !s.expired(secs, utc));
        self.pending
            .lock()
            .expect("pending lock poisoned")
            .retain(|_, p| p.born.elapsed() < PENDING);
    }
}

fn redeem(
    pending: &mut HashMap<String, Pending>,
    state: &str,
    held: Option<&str>,
    at: Instant,
) -> Option<String> {
    if !held.is_some_and(|held| super::same(held, state)) {
        return None;
    }
    let found = pending.remove(state)?;
    (at.duration_since(found.born) < PENDING).then_some(found.verifier)
}

fn digest(id: &str) -> [u8; 32] {
    Sha256::digest(id.as_bytes()).into()
}

/// One cookie's value, by name.
pub(super) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|jar| jar.split(';'))
        .filter_map(|crumb| crumb.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_string())
}

/// A `__Host-` cookie: https only, this host only, no script can read it.
/// `Lax` rather than `Strict` because the sign-in comes back from
/// accounts.google.com, and a `Strict` cookie is not sent on that navigation.
fn set(name: &str, value: &str, max_age: u64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={max_age}"
    ))
    .expect("cookie values here are ASCII")
}

fn unset(name: &str) -> HeaderValue {
    set(name, "", 0)
}

pub(super) fn palette_cookie(name: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{PALETTE}={name}; Path=/; Secure; SameSite=Lax; Max-Age=31536000"
    ))
    .expect("palette names are ASCII")
}

// ------------------------------------------------------------- the context ---

/// Whose request this is, as far as the pages are concerned.
///
/// `Local` is the machine `craft serve` runs on, and every method below does
/// exactly what the pages did before there was a hosted mode. `Session` is a
/// visitor to a hosted server, and nothing it answers comes off the disk.
#[derive(Clone)]
pub(super) enum Ctx {
    Local,
    Session(Arc<Session>),
}

impl Ctx {
    pub fn has_tokens(&self) -> Result<bool> {
        match self {
            Ctx::Local => Ok(Tokens::load()?.is_some()),
            Ctx::Session(_) => Ok(true),
        }
    }

    pub fn account(&self) -> Result<Option<Account>> {
        match self {
            Ctx::Local => Auth::account(),
            Ctx::Session(s) => Ok(Some(s.account.clone())),
        }
    }

    pub fn config(&self) -> Result<Config> {
        match self {
            Ctx::Local => Config::load(),
            Ctx::Session(s) => Ok(s.cfg.lock().expect("config lock poisoned").clone()),
        }
    }

    pub fn save(&self, cfg: Config) -> Result<()> {
        match self {
            Ctx::Local => cfg.save(),
            Ctx::Session(s) => {
                *s.cfg.lock().expect("config lock poisoned") = cfg;
                Ok(())
            }
        }
    }

    /// The plan this visitor is on. Local asks the way every command does;
    /// a session asks the service about its own account, and nobody else's.
    pub async fn tier(&self) -> Result<Option<Tier>> {
        match self {
            Ctx::Local => Ok(license::sync(&Config::load()?).await),
            Ctx::Session(s) => {
                let cached = *s.tier.lock().expect("tier lock poisoned");
                if let Some((at, Some(tier))) = cached {
                    if at.elapsed() < PAID_FOR {
                        return Ok(Some(tier));
                    }
                }
                match license::lookup(&s.account).await {
                    Ok(tier) => {
                        *s.tier.lock().expect("tier lock poisoned") = Some((Instant::now(), tier));
                        Ok(tier)
                    }
                    // Unreachable is not unsubscribed: the last answer stands.
                    Err(err) => match cached {
                        Some((_, tier)) => Ok(tier),
                        None => Err(err),
                    },
                }
            }
        }
    }

    pub fn ga(&self) -> Result<Ga> {
        match self {
            Ctx::Local => Ga::new(),
            Ctx::Session(s) => Ga::with(s.web.clone(), Store::Memory(s.tokens.clone())),
        }
    }

    pub fn hosted(&self) -> bool {
        matches!(self, Ctx::Session(_))
    }
}

// ---------------------------------------------------------------- the chrome ---

/// What the page shell needs to know about the request it is rendering,
/// set per request rather than read from a process-wide setting: on a hosted
/// server the palette is one visitor's choice, not the machine's.
#[derive(Clone, Default)]
pub(super) struct Chrome {
    pub pal: Option<String>,
    pub hosted: bool,
}

tokio::task_local! {
    pub(super) static CHROME: Chrome;
}

// ---------------------------------------------------------------- the guards ---

/// In front of every route on a hosted server: the origin check on anything
/// that writes, the palette, and the headers a page on the public web wants.
pub(super) async fn outer(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let hosted = app
        .hosted
        .as_ref()
        .expect("the hosted guard runs on a hosted server");

    // A browser always names its origin on a POST. One that names another,
    // or none, is not this server's own page submitting a form. `/v1/mcp` is
    // the exception: its callers are assistants' servers, which name no
    // origin, and what it answers to is the token in the request — never a
    // cookie a forged form could ride on.
    let connector = request.uri().path() == "/v1/mcp";
    if !connector && request.method() != Method::GET && request.method() != Method::HEAD {
        let origin = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok());
        if origin != Some(hosted.public.as_str()) {
            return (
                StatusCode::FORBIDDEN,
                "cross-origin requests are not answered here",
            )
                .into_response();
        }
    }

    let pal = cookie(request.headers(), PALETTE).filter(|name| known_palette(name));
    let chrome = Chrome { pal, hosted: true };
    let mut response = CHROME.scope(chrome, next.run(request)).await;

    let headers = response.headers_mut();
    headers.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=31536000"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

/// In front of the views that need somebody signed in. With no live session
/// the answer is the sign-in page — not an error, since that is exactly where
/// somebody with an expired session should be.
pub(super) async fn sessioned(
    State(app): State<Arc<App>>,
    mut request: Request,
    next: Next,
) -> Response {
    let hosted = app
        .hosted
        .as_ref()
        .expect("the hosted guard runs on a hosted server");
    // A demo signs nobody in, and every handler answers it before reaching
    // for anything a session would hold.
    let ctx = if app.demo {
        Ctx::Local
    } else {
        match hosted.find(request.headers()) {
            Some(session) => Ctx::Session(session),
            None => return Redirect::to("/signin").into_response(),
        }
    };
    request.extensions_mut().insert(ctx);
    next.run(request).await
}

pub(super) fn known_palette(name: &str) -> bool {
    crate::theme::THEMES.iter().any(|p| p.name == name)
}

// ---------------------------------------------------------------- signing in ---

/// `/`, hosted: signed in goes wherever they stand; not goes to sign in.
pub(super) fn signed_in(app: &App, headers: &HeaderMap) -> Option<Ctx> {
    let hosted = app.hosted.as_ref()?;
    if app.demo {
        return Some(Ctx::Local);
    }
    hosted.find(headers).map(Ctx::Session)
}

/// `POST /signin`, hosted: off to Google, with the state in a cookie.
pub(super) async fn start(State(app): State<Arc<App>>) -> Response {
    begin(&app, false)
}

/// `GET /signin/consent`: the same trip, with Google's consent screen forced
/// — where a sign-in lands when the MCP connector needs a refresh token and
/// Google, having been approved before, did not send one.
pub(super) async fn start_consent(State(app): State<Arc<App>>) -> Response {
    begin(&app, true)
}

fn begin(app: &App, consent: bool) -> Response {
    let hosted = app.hosted.as_ref().expect("hosted route");
    if app.demo {
        return Redirect::to("/").into_response();
    }
    let auth::Pkce {
        verifier,
        challenge,
    } = auth::pkce();
    let state = auth::nonce(32);
    let url = match hosted.auth(Store::Memory(Arc::default())) {
        Ok(auth) => auth.authorize_url(
            &hosted.redirect_uri(),
            &state,
            &challenge,
            hosted.grants.is_some(),
            consent,
        ),
        Err(err) => return (StatusCode::SERVICE_UNAVAILABLE, err.to_string()).into_response(),
    };
    hosted
        .pending
        .lock()
        .expect("pending lock poisoned")
        .insert(
            state.clone(),
            Pending {
                verifier,
                born: Instant::now(),
            },
        );

    let mut response = Redirect::to(&url).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, set(OAUTH, &state, PENDING.as_secs()));
    response
}

#[derive(Deserialize)]
pub(super) struct Callback {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Why a sign-in did not end in a session, in words for the sign-in page.
pub(super) enum Refused {
    Cancelled,
    Stale,
    Scopes,
    Google(String),
}

impl Refused {
    pub fn say(&self) -> String {
        match self {
            Refused::Cancelled => "the sign-in was cancelled on Google's side.".into(),
            Refused::Stale => {
                "that sign-in had expired or was started in another browser — try again.".into()
            }
            Refused::Scopes => "anacraft needs both Analytics boxes ticked on Google's screen — \
                 one to read the numbers and one to create the property. Try again and leave both on."
                .into(),
            Refused::Google(why) => why.clone(),
        }
    }
}

/// `GET /oauth/callback`: the code, traded for a session.
pub(super) async fn callback(
    app: &App,
    headers: &HeaderMap,
    back: Callback,
) -> std::result::Result<Response, Refused> {
    let hosted = app.hosted.as_ref().expect("hosted route");
    if back.error.is_some() {
        return Err(Refused::Cancelled);
    }
    let (Some(code), Some(state)) = (back.code, back.state) else {
        return Err(Refused::Stale);
    };
    let held = cookie(headers, OAUTH);
    let verifier = hosted
        .redeem(&state, held.as_deref())
        .ok_or(Refused::Stale)?;

    let auth = hosted
        .auth(Store::Memory(Arc::default()))
        .map_err(|err| Refused::Google(err.to_string()))?;
    let tokens = auth
        .exchange(&code, &verifier, &hosted.redirect_uri())
        .await
        .map_err(|err| Refused::Google(super::views::why(err)))?;

    if !tokens.granted(auth::SCOPE_READ) || !tokens.granted(SCOPE_EDIT) {
        return Err(Refused::Scopes);
    }
    let account = tokens
        .account
        .clone()
        .ok_or_else(|| Refused::Google("Google did not say which account signed in.".into()))?;

    // Keep the grant the connector reads with. Google sends a refresh token
    // only through the consent screen, so a returning visitor usually comes
    // back without one — fine if one is already stored, and otherwise one
    // more trip, with consent, to get it.
    if let Some(grants) = &hosted.grants {
        if tokens.refresh_token.is_empty() {
            if !grants.has(&account).await.unwrap_or(true) {
                return Ok(Redirect::to("/signin/consent").into_response());
            }
        } else if let Err(err) = grants.save(&account, &tokens.refresh_token).await {
            // The pages work without it; only the connector would not. Said
            // on the server's log, where somebody can act on it.
            eprintln!("  could not store a grant: {err}");
        }
    }

    // The same courtesy `craft login` does, off the request's path: register
    // the account so a subscription bought anywhere finds it.
    let linked = account.clone();
    tokio::spawn(async move {
        let _ = license::link(&linked).await;
    });

    let id = hosted
        .open(account, tokens)
        .map_err(|err| Refused::Google(err.to_string()))?;
    let mut response = Redirect::to("/").into_response();
    let jar = response.headers_mut();
    jar.append(header::SET_COOKIE, set(SESSION, &id, IDLE * 2));
    jar.append(header::SET_COOKIE, unset(OAUTH));
    Ok(response)
}

/// `POST /signout`, hosted: forget the session. Google's grant is left alone
/// — revoking it would sign this person out of every other device too.
pub(super) fn close(app: &App, headers: &HeaderMap) -> Response {
    if let Some(hosted) = &app.hosted {
        hosted.close(headers);
    }
    let mut response = Redirect::to("/").into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, unset(SESSION));
    response
}

// ------------------------------------------------------------ the connector ---

#[derive(Deserialize)]
pub(super) struct Wire {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    property: Option<String>,
}

/// `POST /v1/mcp` on a hosted server: an assistant, with a connector link,
/// asking about one user's property. The link's token says whose; the grant
/// store says with what.
pub(super) async fn mcp(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Query(wire): axum::extract::Query<Wire>,
    body: axum::body::Bytes,
) -> Response {
    let hosted = app.hosted.as_ref().expect("hosted route");
    let refuse = |status: StatusCode, message: &str| {
        (
            status,
            axum::Json(serde_json::json!({ "error": { "message": message } })),
        )
            .into_response()
    };
    let Some(grants) = &hosted.grants else {
        return refuse(
            StatusCode::NOT_FOUND,
            "this server has no MCP connector switched on",
        );
    };
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
        .or(wire.token)
        .unwrap_or_default();
    if token.is_empty() {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "this connector wants its token — copy the link again from app.anacraft.dev",
        );
    }
    let message: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(err) => {
            return axum::Json(serde_json::json!({
                "jsonrpc": "2.0", "id": null,
                "error": { "code": -32700, "message": format!("invalid JSON: {err}") },
            }))
            .into_response()
        }
    };
    let property = wire.property.map(|p| super::bare(&p)).unwrap_or_default();
    let key = format!("{}:{property}", hex(&digest(&token)));

    let server = {
        let mut built = hosted.mcp.lock().await;
        let fresh = built.get(&key).and_then(|b| {
            let usable = b.at.elapsed() < REBUILD
                && b.server
                    .try_lock()
                    .map_or(true, |s| s.lock_reason().is_none());
            usable.then(|| b.server.clone())
        });
        match fresh {
            Some(server) => server,
            None => {
                let grant = match grants.open(&token).await {
                    Ok(Some(grant)) => grant,
                    Ok(None) => {
                        return refuse(
                            StatusCode::UNAUTHORIZED,
                            "that connector link is not one this server knows — it may have \
                             been turned off. Copy it again from app.anacraft.dev",
                        )
                    }
                    Err(err) => return refuse(StatusCode::BAD_GATEWAY, &err.to_string()),
                };
                let server = match build(hosted, grant, &property).await {
                    Ok(server) => Arc::new(tokio::sync::Mutex::new(server)),
                    Err(err) => return refuse(StatusCode::BAD_GATEWAY, &err.to_string()),
                };
                built.insert(
                    key,
                    Built {
                        at: Instant::now(),
                        server: server.clone(),
                    },
                );
                server
            }
        }
    };
    let mut server = server.lock().await;
    super::answer(&mut server, message).await
}

/// One user's MCP server: their stored grant, their plan, their property.
async fn build(
    hosted: &Hosted,
    grant: super::grants::Grant,
    property: &str,
) -> Result<crate::mcp::Server> {
    let web = hosted.web.clone().context("this server signs nobody in")?;
    // An access token already expired, so the first call refreshes with the
    // stored grant — the same path a session's Auth takes.
    let tokens = Tokens {
        access_token: String::new(),
        refresh_token: grant.refresh_token,
        expires_at: chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        account: Some(grant.account.clone()),
        scope: Some(format!("{} {SCOPE_EDIT}", auth::SCOPE_READ)),
    };
    let ga = Ga::with(web, Store::Memory(Arc::new(Mutex::new(Some(tokens)))))?;
    let tier = license::lookup(&grant.account).await.ok().flatten();
    let mut cfg = Config::default();
    let mut property = property.to_string();
    if property.is_empty() {
        // A link that names no property — an older one, or a client that
        // kept the token and dropped the rest. The account says what there
        // is to read: one property is unambiguous; several are the
        // assistant's to choose between, with `list_properties` and the
        // `property` argument every tool takes.
        if let Ok(found) = ga.properties().await {
            if let [only] = found.as_slice() {
                property = only.id.clone();
            }
            for p in found {
                cfg.upsert(&p.id, Some(p.name));
            }
        }
    } else {
        cfg.upsert(&property, None);
    }
    Ok(crate::mcp::build_for(
        cfg,
        ga,
        tier,
        Some(property.as_str()).filter(|p| !p.is_empty()),
    ))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `POST /connect/off`: forget this user's stored grant. Every connector link
/// they handed out stops answering, and the refresh token is gone from the
/// table; signing in again turns it back on.
pub(super) async fn connector_off(app: &App, account: &Account) -> Result<()> {
    let hosted = app.hosted.as_ref().expect("hosted route");
    if let Some(grants) = &hosted.grants {
        grants.forget(account).await?;
        hosted.mcp.lock().await.clear();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(expires_in: i64) -> Tokens {
        Tokens {
            access_token: "a".into(),
            refresh_token: String::new(),
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(expires_in),
            account: None,
            scope: None,
        }
    }

    fn session(sub: &str) -> Session {
        Session {
            account: Account {
                sub: sub.into(),
                email: None,
            },
            web: ClientCreds {
                client_id: "id".into(),
                client_secret: "secret".into(),
            },
            tokens: Arc::new(Mutex::new(Some(tokens(3600)))),
            cfg: Mutex::new(Config::default()),
            tier: Mutex::new(None),
            last: AtomicU64::new(now()),
        }
    }

    #[test]
    fn an_oauth_state_is_one_shot_bound_to_its_cookie_and_expires() {
        let born = Instant::now();
        let fill = || {
            let mut pending = HashMap::new();
            pending.insert(
                "s1".to_string(),
                Pending {
                    verifier: "v1".into(),
                    born,
                },
            );
            pending
        };

        let mut pending = fill();
        assert_eq!(redeem(&mut pending, "s1", None, born), None, "no cookie");
        assert_eq!(
            redeem(&mut pending, "s1", Some("s2"), born),
            None,
            "another browser's cookie"
        );
        assert_eq!(
            redeem(&mut pending, "s1", Some("s1"), born).as_deref(),
            Some("v1")
        );
        assert_eq!(
            redeem(&mut pending, "s1", Some("s1"), born),
            None,
            "only once"
        );

        let mut pending = fill();
        assert_eq!(
            redeem(&mut pending, "s1", Some("s1"), born + PENDING),
            None,
            "too old"
        );
    }

    #[test]
    fn a_session_dies_idle_or_when_its_token_does() {
        let utc = chrono::Utc::now();
        let live = session("a");
        assert!(!live.expired(now(), utc));
        assert!(live.expired(now() + IDLE + 1, utc), "idle");

        let spent = session("b");
        *spent.tokens.lock().unwrap() = Some(tokens(-1));
        assert!(spent.expired(now(), utc), "token past its expiry");
    }

    #[test]
    fn two_sessions_never_share_tokens_or_a_property() {
        let a = Arc::new(session("a"));
        let b = Arc::new(session("b"));
        *b.tokens.lock().unwrap() = Some(Tokens {
            access_token: "b-token".into(),
            ..tokens(3600)
        });
        let (ca, cb) = (Ctx::Session(a.clone()), Ctx::Session(b.clone()));

        let mut cfg = ca.config().unwrap();
        cfg.upsert("111", Some("site a".into()));
        ca.save(cfg).unwrap();
        assert!(ca.config().unwrap().find("111").is_some());
        assert!(cb.config().unwrap().find("111").is_none());

        let store_a = Store::Memory(a.tokens.clone());
        assert_eq!(store_a.load().unwrap().unwrap().access_token, "a");
        assert_eq!(cb.account().unwrap().unwrap().sub, "b");
    }

    #[test]
    fn the_session_cookie_is_host_prefixed_secure_httponly_and_lax() {
        let value = set(SESSION, "abc", 60);
        let value = value.to_str().unwrap();
        assert!(value.starts_with("__Host-craft_session=abc;"));
        for part in ["Path=/", "Secure", "HttpOnly", "SameSite=Lax"] {
            assert!(value.contains(part), "{value} lacks {part}");
        }
        assert!(
            !value.contains("Domain"),
            "a __Host- cookie names no domain"
        );
    }

    #[test]
    fn a_cookie_is_found_by_its_whole_name() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("x__Host-craft_session=no; __Host-craft_session=yes"),
        );
        assert_eq!(cookie(&headers, SESSION).as_deref(), Some("yes"));
    }
}
