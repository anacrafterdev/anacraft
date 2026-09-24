//! `craft serve` — the register-a-tag flow, as an API on this machine.
//!
//! The commands this binary already has each answer one question in one voice:
//! a terminal's. `serve` is the same work with the voice taken off — sign in,
//! find or make a property, read the measurement id off its web stream, hand
//! back the tag — so that the thing asking can be a page in a browser, an
//! editor extension, or a script that has never seen a panel border.
//!
//! Nothing here is a second implementation. The property work is
//! [`crate::configure::setup`], which `craft configure` and the MCP tool both
//! already call; the reports are the same functions [`crate::mcp`] serves; the
//! plan check is [`crate::license::gate`]. What this module owns is the HTTP:
//! who is allowed to ask, what an answer looks like, and what a refusal says.
//!
//! It binds loopback and nothing else. Every route but `/v1/health` and the
//! way into the pages wants this machine's token — derived, not minted, so a
//! connector configured once stays configured — and
//! a browser reaching it has to come from this server's own origin. That is
//! three locks on a door that only opens onto one machine, and they are there
//! because a page on the public web can absolutely try to talk to
//! `127.0.0.1` — it just cannot guess forty random characters while doing it.
//!
//! The pages themselves live in [`views`]. They are routes rather than one
//! file with a script in it, they take the token in a cookie rather than a
//! header because a `<form>` cannot send a header, and they call the same
//! functions the handlers below do.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

mod grants;
mod hosted;
mod views;

use hosted::Ctx;

use crate::auth::{Auth, Cta, Landing, Tokens};
use crate::config::Config;
use crate::ga::Ga;
use crate::license::{self, Tier};

/// The API reference at `/docs`, drawn from the document this server
/// generates rather than from a second list of routes that could fall behind
/// it.
///
/// Swagger UI, Redoc and Scalar all do this better, and all of them are a
/// megabyte of JavaScript fetched from a CDN — a page that stops working on a
/// plane, and a megabyte of binary for a reference to nineteen routes. This is
/// nine kilobytes and needs nothing.
const API_PAGE: &str = include_str!("../assets/api.html");

/// How long a request may take to reach us before the server decides nobody is
/// coming back. Checked on a timer rather than per request, so the granularity
/// is the timer's.
const IDLE_TICK: Duration = Duration::from_secs(20);

/// The plan `craft serve` is part of.
///
/// Elite, the same one `craft mcp` is on, because it is the same kind of
/// thing: the binary standing up a service for something else to talk to,
/// rather than printing an answer to the person who typed the command. One
/// plan covers both, so somebody wiring a Lovable app into an assistant is
/// never asked to reason about two.
///
/// It gates the API, not the command. `craft serve` starts for anybody — the
/// page it opens is where somebody signs in and, if they need to, subscribes,
/// and a server that refused to start would leave them nowhere to do either.
const PLAN: Tier = Tier::Elite;

/// Where this server is reachable, and the one ingredient its token is not
/// derived from.
///
/// `craft serve` used to mint forty random characters and take whatever port
/// the OS offered. That is right for a browser session, which reads both off
/// the banner and forgets them when the tab closes. It is wrong for
/// `/v1/mcp`: a connector is configured once, by hand, in a file somebody
/// then stops thinking about, and a URL or a token that changed overnight is
/// a connector that quietly stopped working — with nothing in the client to
/// say why.
///
/// So neither is left to chance. The port is the one last served from. The
/// token is [`derive`]d from the signed-in account and the property, which
/// are stable but public, under the secret below, which is neither — minted
/// once and never shown. It lives beside the credentials at 0600 rather than
/// in `~/.config`, because it is a secret and `~/.config` is a directory
/// people sync to public repos.
#[derive(Default, Serialize, Deserialize)]
struct Endpoint {
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
}

/// The port to try first: the one named, then the one this server was last
/// reachable at, then whatever the OS has going spare.
///
/// The middle case is the whole point. A connector holds a URL with a port in
/// it, and an OS-assigned port that differs every run makes that URL a guess.
fn wanted_port(given: u16, remembered: Option<u16>) -> u16 {
    match given {
        0 => remembered.unwrap_or(0),
        port => port,
    }
}

impl Endpoint {
    fn path() -> Result<std::path::PathBuf> {
        Ok(crate::config::home()?.join("mcp-http.json"))
    }

    /// Best effort throughout. Every way this can fail — no file yet, no home
    /// directory, JSON somebody edited by hand — means the same thing:
    /// nothing is remembered, so mint and bind as if this were the first run.
    fn load() -> Endpoint {
        Self::path()
            .ok()
            .filter(|path| path.exists())
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// Also best effort: failing to write this file down is not a reason to
    /// refuse to serve. The caller has the port and the token either way —
    /// they are on the banner.
    fn save(&self) {
        if let (Ok(path), Ok(raw)) = (Self::path(), serde_json::to_string_pretty(self)) {
            let _ = crate::config::write_private(&path, &raw);
        }
    }

    /// The key every token on this machine is derived under, minted on first
    /// use and kept forever after.
    ///
    /// Losing it is survivable and obvious: every derived token changes at
    /// once, and the connectors are rewired the same way they were wired.
    /// Leaking it is the thing the 0600 is for.
    fn secret(&mut self) -> String {
        if let Some(secret) = &self.secret {
            return secret.clone();
        }
        let secret = license::mint_token();
        self.secret = Some(secret.clone());
        self.save();
        secret
    }

    /// Write the port back, unless it is already what is on disk.
    fn remember(&mut self, port: u16) {
        if self.port == Some(port) {
            return;
        }
        self.port = Some(port);
        self.save();
    }
}

/// The bearer token for one account and one property.
///
/// HMAC rather than a hash of the two ids, because neither id is a secret: a
/// Google `sub` rides in every id token, and a GA4 property id is printed in
/// the tag on every page of the site it measures. Hashing them would put the
/// key to this server behind two strings that are already published. The
/// secret is what makes the token unguessable; the ids are what make it the
/// same one tomorrow.
///
/// Twenty bytes, spelled in hex, which is the forty characters the minted
/// token was — the same length in the same places, so nothing downstream has
/// to learn a new shape.
fn derive(secret: &str, account: &str, property: &str) -> String {
    use hmac::Mac;

    let mut mac = <hmac::Hmac<sha2::Sha256>>::new_from_slice(secret.as_bytes())
        .expect("hmac accepts a key of any length");
    // Length-prefixed rather than joined on a separator, so no pair of ids
    // can be rearranged into another pair with the same token. Neither field
    // can contain a digit-then-colon prefix of its own length by accident,
    // but the cost of not having to argue about it is one `format!`.
    mac.update(format!("{}:{account}{}:{property}", account.len(), property.len()).as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .take(20)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Every token this server answers to, and the property each one names.
///
/// One per property, and not only the active one, because switching property
/// is a thing people do from the dashboard between breakfast and lunch — and
/// a connector wired up yesterday against the property that was active then
/// should not start refusing because of it. Answering to all of them widens
/// nothing: they are one account's, and whoever holds any of these tokens
/// already holds the secret they were derived under.
///
/// The active property is first, because that is the token this server
/// advertises on its banner and its page.
fn keyring(
    secret: &str,
    account: &str,
    active: &str,
    configured: &[String],
) -> Vec<(String, String)> {
    let mut ring: Vec<(String, String)> = Vec::new();
    for id in std::iter::once(active.to_string()).chain(configured.iter().cloned()) {
        if ring.iter().any(|(known, _)| known == &id) {
            continue;
        }
        let token = derive(secret, account, &id);
        ring.push((id, token));
    }
    ring
}

/// What a presented token turned out to be.
enum Opened {
    /// It was derived for this property, so this is the property its holder
    /// is asking about — whatever the dashboard has in front of it.
    Property(String),
    /// It is the token this server was started with under `--token`, which
    /// names no property. The reads fall back to the flag and the config, the
    /// way they did before any of this was derived from anything.
    Server,
}

/// How this server decides whether a request may pass, and what it is about.
enum Keys {
    /// `--token`: one token, named by the caller.
    Named(String),
    /// The derived ring.
    Derived { secret: String, account: String },
}

impl Keys {
    /// The ring as it stands right now.
    ///
    /// Worked out per request rather than at startup, because the config it
    /// is derived from changes while the server is running: registering a
    /// property on the page hands out a token for it, and a token that only
    /// worked after a restart would be a button that does nothing.
    /// It is a small TOML read and a handful of HMACs, in front of handlers
    /// that are about to call Google.
    fn ring(&self, flag: Option<&str>) -> Vec<(String, String)> {
        match self {
            Keys::Named(token) => vec![(String::new(), token.clone())],
            Keys::Derived { secret, account } => {
                let (_, active, configured) = identity(flag);
                keyring(secret, account, &active, &configured)
            }
        }
    }

    /// What the presented token opens, if anything.
    fn opens(&self, presented: &str, flag: Option<&str>) -> Option<Opened> {
        opened(&self.ring(flag), presented)
    }
}

/// Which of a ring's tokens was presented, and what it names.
///
/// Split out from [`Keys::opens`] because that one reads the config to build
/// its ring, and the rule being applied to the ring is worth a test that does
/// not depend on what is in the config of the machine running it.
fn opened(ring: &[(String, String)], presented: &str) -> Option<Opened> {
    ring.iter()
        .find(|(_, token)| same(presented, token))
        .map(|(id, _)| {
            if id.is_empty() {
                Opened::Server
            } else {
                Opened::Property(id.clone())
            }
        })
}

/// The three strings an MCP client is configured with, spelled out rather
/// than left as an exercise.
///
/// The clients this endpoint exists for are the ones that cannot spawn `craft
/// mcp`: a confined snap, a Mac App Store build, a container. Wiring one up
/// means copying strings into a sandboxed app's config file, and a URL
/// somebody has to assemble from a line of output and a path they read in the
/// docs is one they will get wrong once.
pub struct Connector {
    /// The endpoint on its own, for a client with a field for the token.
    pub url: String,
    pub token: String,
    /// Both halves in one string.
    ///
    /// The thing most of these clients actually have room for is a URL, and
    /// nothing else — no header, no second field. A token in a query string
    /// is normally a thing to avoid, because query strings end up in access
    /// logs and `Referer` headers; this one goes to a server on loopback that
    /// keeps no log and is never a web page's origin, and it was going to sit
    /// in the client's config file either way. So it is offered, and it is
    /// the one to copy.
    pub link: String,
    /// Ready to paste into a shell, for the clients that take it that way.
    pub command: String,
}

impl Connector {
    fn new(port: u16, token: String) -> Connector {
        Connector::at(format!("http://127.0.0.1:{port}/v1/mcp"), token, None)
    }

    /// The same three strings for any endpoint — a hosted server's, where the
    /// token is the user's and the property rides in the link.
    fn at(url: String, token: String, property: Option<&str>) -> Connector {
        let mut link = format!("{url}?token={}", license::encode(&token));
        if let Some(property) = property.filter(|p| !p.is_empty()) {
            link.push_str(&format!("&property={}", license::encode(property)));
        }
        Connector {
            // One line, no continuation. It is long, and it is going through
            // a clipboard into a shell or a config file — neither of which is
            // improved by a backslash somebody has to keep. Quoted, because
            // the `?` in it is a glob to every shell that will see it.
            command: format!("claude mcp add --transport http anacraft '{link}'"),
            url,
            token,
            link,
        }
    }
}

/// The connector for one property on this machine, whether or not `craft
/// serve` is running.
///
/// The dashboard calls this: somebody looking at a property's numbers is
/// exactly the person who wants an assistant looking at them too, and the
/// alternative is starting a server, reading a banner and retyping it.
/// Because the port and the token are both settled off disk, the answer is
/// the same one `craft serve` will print — even on a machine where it has
/// never run, since picking the port here is also remembering it for when it
/// does.
pub fn connector(property: &str) -> Result<Connector> {
    let mut endpoint = Endpoint::load();

    let port = match endpoint.port {
        Some(port) => port,
        None => {
            // Bound and dropped, purely to be told a number nothing else is
            // using. `run` re-binds it, and falls back if the gap between the
            // two was long enough for somebody to take it.
            let port = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .and_then(|listener| listener.local_addr())
                .context("could not find a free local port")?
                .port();
            endpoint.remember(port);
            port
        }
    };

    let (account, active, _) = identity(None);
    let property = if property.is_empty() {
        active
    } else {
        property.to_string()
    };
    Ok(Connector::new(
        port,
        derive(&endpoint.secret(), &account, &property),
    ))
}

/// Who this machine is signed in as, which property is in front of them, and
/// which others they have configured.
///
/// Every part is allowed to be missing, and missing is spelled as the empty
/// string rather than refused. A token derived from nothing at all is still a
/// token nobody can guess — the secret is doing that work — and refusing to
/// serve because somebody has not signed in yet would be refusing to open the
/// page where they sign in.
fn identity(flag: Option<&str>) -> (String, String, Vec<String>) {
    let account = crate::auth::Auth::account()
        .ok()
        .flatten()
        .map(|account| account.sub)
        .unwrap_or_default();

    let cfg = crate::config::Config::load().unwrap_or_default();
    let configured: Vec<String> = cfg.properties.iter().map(|p| p.id.clone()).collect();
    let active = flag
        .map(str::to_string)
        .or_else(|| cfg.active.clone())
        .or_else(|| configured.first().cloned())
        .unwrap_or_default();

    (account, active, configured)
}

pub struct Options {
    /// 0 means the port this server was last reachable at, and the OS's
    /// choice on the first run ever — see [`Endpoint`]. Naming one here
    /// outranks both.
    pub port: u16,
    /// Open a browser at the page. Off with `--no-open`.
    pub open: bool,
    /// Supply the bearer token, for a caller that has to know it in advance.
    pub token: Option<String>,
    /// Minutes with no request before the server stops. 0 runs until Ctrl-C.
    pub idle: u64,
    /// Synthetic data on the read endpoints, and no account needed.
    pub demo: bool,
    /// `--property`, as the default for reads that name none.
    pub property: Option<String>,
    /// Where to listen. Loopback unless the server is hosted.
    pub host: IpAddr,
    /// `--public-url`: the https address this server is reached at, which
    /// makes it a hosted server — many visitors, a session each, nothing on
    /// disk. See [`hosted`].
    pub public_url: Option<String>,
}

/// What a sign-in started through the API is doing right now.
///
/// The OAuth flow blocks on a loopback accept, so it runs on a thread of its
/// own and reports back through here. `GET /v1/session` is what reads it.
enum Login {
    Idle,
    Pending,
    Failed(String),
}

struct App {
    /// The one this server advertises: the active property's. The pages use
    /// it as the browser session's key; the ring below is what the API takes.
    token: String,
    /// What the API answers to, and what each token turns out to mean.
    keys: Keys,
    /// The one origin a browser may call from — this server's own.
    origin: String,
    /// The port behind that origin, so the page that hands out the connector
    /// can build the same line the banner printed.
    port: u16,
    demo: bool,
    property: Option<String>,
    login: Mutex<Login>,
    /// Unix seconds of the last request, for the idle clock.
    last: AtomicU64,
    /// The MCP server behind `/v1/mcp`, built on the first call rather than at
    /// startup: building it syncs the subscription and opens a GA4 client, and
    /// `craft serve` starts for people who have neither yet — the page it opens
    /// is where they sign in. A `tokio` mutex because `dispatch` is async and
    /// holds `&mut self` across awaits; the stdio transport is serial too, so
    /// one call at a time is the shape this server already had.
    /// Paired with the property it was built for, because the token names
    /// one and two connectors on this server may name two different ones.
    mcp: tokio::sync::Mutex<Option<(String, crate::mcp::Server)>>,
    /// Present when this is a hosted server, and then the only place a
    /// visitor's sign-in is kept.
    hosted: Option<hosted::Hosted>,
}

impl App {
    /// What a presented token opens here, if anything.
    fn opens(&self, presented: &str) -> Option<Opened> {
        self.keys.opens(presented, self.property.as_deref())
    }

    /// The token a client should present to read this property.
    ///
    /// Under `--token` there is only the one the caller named, and it names
    /// no property — so every property's page shows that, which is the truth:
    /// it is what opens the door, and the door leads to whichever property
    /// the flag and the config picked.
    pub(super) fn token_for(&self, property: &str) -> String {
        match &self.keys {
            Keys::Named(token) => token.clone(),
            // The demo serves one synthetic property whatever the config
            // says, and its id is not in the config at all — so deriving one
            // for it would hand out a token this server does not answer to.
            // There is one token here, and it is the one on the banner.
            Keys::Derived { .. } if self.demo => self.token.clone(),
            Keys::Derived { secret, account } => derive(secret, account, property),
        }
    }

    /// Whether that token is the same one whatever property is asked for.
    pub(super) fn one_token(&self) -> bool {
        self.demo || matches!(self.keys, Keys::Named(_))
    }
}

pub async fn run(opts: Options) -> Result<()> {
    if let Some(public) = opts.public_url.clone() {
        return run_hosted(opts, public).await;
    }
    if !opts.host.is_loopback() {
        anyhow::bail!(
            "--host {} would put this machine's token-guarded server on the network. \
             A server other people reach is a hosted one: add --public-url https://…",
            opts.host
        );
    }
    let mut endpoint = Endpoint::load();

    let wanted = wanted_port(opts.port, endpoint.port);
    let mut squatted = false;
    let listener = match tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, wanted)).await {
        Ok(listener) => listener,
        // Something else has the remembered port — most often another copy of
        // this server, still running. That is not a reason to refuse to
        // start: take whatever is free. A port the caller named is a
        // different matter — they meant that one, and being given another
        // silently would be worse.
        Err(_) if opts.port == 0 && wanted != 0 => {
            squatted = true;
            tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .context("could not open a local port")?
        }
        Err(err) => {
            return Err(err).with_context(|| match opts.port {
                0 => "could not open a local port".to_string(),
                port => format!("could not open port {port} — something else may have it"),
            })
        }
    };
    let port = listener.local_addr()?.port();

    // Not the fallback, though. The connectors already out there point at
    // the remembered port, and writing a port taken around a squatter into
    // the file would move every one of them the next time this starts alone.
    // The banner and the page say where this run actually is.
    if !squatted {
        endpoint.remember(port);
    }

    // The token is derived, not minted: same account and same property, same
    // forty characters, run after run. `--token` still outranks it — a caller
    // that has to know the token in advance is naming it, not asking.
    let (account, active, configured) = identity(opts.property.as_deref());
    let keys = match opts.token {
        Some(token) => Keys::Named(token),
        None => Keys::Derived {
            secret: endpoint.secret(),
            account: account.clone(),
        },
    };
    let token = match &keys {
        Keys::Named(token) => token.clone(),
        Keys::Derived { secret, account } => keyring(secret, account, &active, &configured)
            .into_iter()
            .next()
            .map(|(_, token)| token)
            .expect("a keyring is never empty"),
    };

    let app = Arc::new(App {
        token,
        keys,
        origin: format!("http://127.0.0.1:{port}"),
        port,
        demo: opts.demo,
        property: opts.property,
        login: Mutex::new(Login::Idle),
        last: AtomicU64::new(now()),
        mcp: tokio::sync::Mutex::new(None),
        hosted: None,
    });

    // Two routers, because one route has to be reachable without the token:
    // a caller that cannot yet prove anything still deserves to learn whether
    // the server is up, and the page itself is loaded by a browser that keeps
    // the token in a fragment the request never carries.
    let guarded = Router::new()
        .route(
            "/v1/session",
            get(session).post(session_start).delete(session_end),
        )
        .route("/v1/subscription", get(subscription))
        .route("/v1/subscription/checkout", axum::routing::post(checkout))
        .route("/v1/properties", get(properties).post(register))
        .route("/v1/properties/:id", axum::routing::delete(trash))
        .route("/v1/properties/:id/streams", get(streams).post(add_stream))
        .route("/v1/property", put(use_property))
        .route("/v1/themes", get(themes).put(use_theme))
        .route("/v1/tag/:measurement_id", get(tag))
        .route("/v1/overview", get(overview))
        .route("/v1/pages", get(pages))
        .route("/v1/events", get(events))
        .route("/v1/sources", get(sources))
        .route("/v1/referrers", get(referrers))
        .route("/v1/countries", get(countries))
        .route("/v1/live", get(live))
        .route("/v1/audit", get(audit))
        .route("/v1/mcp", axum::routing::post(mcp).get(mcp_no_stream))
        .layer(middleware::from_fn_with_state(app.clone(), guard));

    let router = Router::new()
        // Every page this server renders, and the stylesheet behind them.
        .merge(views::router(app.clone()))
        .route("/v1/health", get(health))
        // Open, like health. A description of the door is not a key to it:
        // this names the routes and the shape of an answer, all of which is
        // published at anacraft.dev/serve.html anyway — and a client
        // generator or an agent reads the description *before* it has been
        // given a token, which is the whole point of there being one.
        .route("/v1/openapi.json", get(openapi))
        // The same document with the punctuation put in. Open for the same
        // reason: it is the reference, and a reference you have to authenticate
        // to read is a reference nobody reads.
        .route("/docs", get(api_page))
        .merge(guarded)
        .with_state(app.clone());

    let url = format!("{}/#k={}", app.origin, app.token);
    banner(
        &app.origin,
        &Connector::new(port, app.token.clone()),
        opts.idle,
        opts.demo,
    );
    if opts.open {
        let _ = open::that(&url);
    }

    axum::serve(listener, router)
        .with_graceful_shutdown(idle(app.clone(), opts.idle))
        .await
        .context("the local server stopped unexpectedly")
}

/// Check `--public-url` and drop its trailing slash. https, or plain http to
/// this machine for trying it out: a session cookie sent in the clear would
/// be a session anybody on the path could spend.
fn public_origin(given: &str) -> Result<String> {
    let url = given.trim().trim_end_matches('/');
    let local = ["http://localhost", "http://127.0.0.1"]
        .iter()
        .any(|base| url == *base || url.starts_with(&format!("{base}:")));
    if !(url.starts_with("https://") || local) {
        anyhow::bail!("--public-url must be https:// (or http://localhost for trying it out)");
    }
    if url.splitn(4, '/').nth(3).is_some() {
        anyhow::bail!("--public-url is an origin — scheme and host, no path");
    }
    Ok(url.to_string())
}

/// The hosted server: the pages and the health check, for anybody, each
/// visitor on a session of their own. No banner, no token, no `~/.anacraft`,
/// and none of `/v1` but health — the API and the MCP connector answer to a
/// token that is one machine's, and that is what they stay.
async fn run_hosted(opts: Options, public: String) -> Result<()> {
    let public = public_origin(&public)?;
    if opts.token.is_some() {
        anyhow::bail!("--token is the local server's; a hosted one signs people in instead");
    }
    // The config every session starts from is its own, but a property named
    // in the environment would be read by every one of them.
    if std::env::var_os("ANACRAFT_PROPERTY_ID").is_some() {
        anyhow::bail!("ANACRAFT_PROPERTY_ID would apply to every visitor; unset it");
    }
    let web = if opts.demo {
        None
    } else {
        Some(crate::auth::ClientCreds::web()?)
    };
    // The MCP connector: on when the grant store is configured, and then a
    // sign-in asks Google for a refresh token to keep, sealed.
    let grants = if opts.demo {
        None
    } else {
        grants::Grants::from_env()?
    };
    let connector = grants.is_some();

    let listener = tokio::net::TcpListener::bind((opts.host, opts.port))
        .await
        .with_context(|| format!("could not listen on {}:{}", opts.host, opts.port))?;
    let port = listener.local_addr()?.port();

    let app = Arc::new(App {
        // Never handed to anybody, and nothing hosted checks it: the routes
        // that take a token are not mounted.
        token: license::mint_token(),
        keys: Keys::Named(license::mint_token()),
        origin: public.clone(),
        port,
        demo: opts.demo,
        property: None,
        login: Mutex::new(Login::Idle),
        last: AtomicU64::new(now()),
        mcp: tokio::sync::Mutex::new(None),
        hosted: Some(hosted::Hosted::new(public.clone(), web, grants)),
    });

    let sweeper = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if let Some(hosted) = &sweeper.hosted {
                hosted.sweep();
            }
        }
    });

    let router = views::hosted_router(app.clone())
        .route("/v1/health", get(health))
        .route(
            "/v1/mcp",
            axum::routing::post(hosted::mcp).get(mcp_no_stream),
        )
        .fallback(local_only)
        .layer(middleware::from_fn_with_state(app.clone(), hosted::outer))
        .with_state(app.clone());

    println!(
        "\n  {} anacraft hosted on {} (listening on {}:{port}){}",
        crate::theme::glyph::PICKAXE,
        public,
        opts.host,
        if opts.demo {
            " — demo"
        } else if connector {
            " — with the MCP connector"
        } else {
            " — no MCP connector (no grant store configured)"
        }
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("the hosted server stopped unexpectedly")
}

/// Anything a hosted server does not serve: the API above all, which lives
/// on the machine that holds its token.
async fn local_only() -> Response {
    Fail::new(
        StatusCode::NOT_FOUND,
        "local_only",
        "nothing here. The API and the MCP connector run on your own machine: \
         install craft and run `craft serve` — https://anacraft.dev/serve.html"
            .into(),
    )
    .into_response()
}

fn banner(origin: &str, wire: &Connector, idle: u64, demo: bool) {
    use crate::render::{bold, dim};
    use crate::theme::glyph;

    println!(
        "\n  {} anacraft serving on {}",
        glyph::PICKAXE,
        bold(origin)
    );
    // The link first, and whole. It is the one string a client that has room
    // for nothing else can be given, and somebody asked to assemble it from a
    // URL on one line and a token on another will get it wrong once.
    println!("  {} {}", dim("mcp"), dim(&wire.link));
    println!("  {} {}", dim("token"), dim(&wire.token));
    println!("\n  {}", dim(&wire.command));
    println!(
        "  {}",
        dim("the same link is on the page, and on `c` in `craft`")
    );
    if demo {
        println!("  {}", dim("synthetic data — no account, no subscription"));
    }
    match idle {
        0 => println!("  {}\n", dim("ctrl-c to stop")),
        mins => println!(
            "  {}\n",
            dim(&format!("ctrl-c to stop, or {mins} idle minutes")),
        ),
    }
}

/// Resolves when the server has had nothing to do for long enough, or when the
/// terminal asks it to stop. A server started to hand over one tag should not
/// still be listening the next morning.
async fn idle(app: Arc<App>, minutes: u64) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    if minutes == 0 {
        return ctrl_c.await;
    }
    let limit = minutes * 60;
    let waiting = async {
        loop {
            tokio::time::sleep(IDLE_TICK).await;
            if now().saturating_sub(app.last.load(Ordering::Relaxed)) >= limit {
                return;
            }
        }
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = waiting => println!("  idle — stopped.\n"),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

// ----------------------------------------------------------- the contract ---

/// Every route this server answers, said once.
///
/// It exists because the alternative is a hand-kept OpenAPI document, and a
/// hand-kept document is a document that is wrong by the third endpoint. This
/// is what `/v1/openapi.json` is built from, and the test at the bottom of
/// this file reads the `.route(` calls out of this module's own source and
/// fails the build if the two disagree — so a route added without a line here
/// does not compile past `cargo test`.
///
/// Rust has generators for this — `utoipa` is the mainstream one, `aide` the
/// other — and both were weighed and left out. They describe a handler by its
/// types, and every handler here answers `Json<Value>`: payloads that are
/// mostly Google's own, passed through rather than modelled. Deriving a schema
/// from `Value` describes nothing, so the annotations would be prose in a
/// macro's clothing, bought with a proc-macro dependency and a rust-version
/// bump to 1.75 on a binary tuned for size. The table is the same prose,
/// checked by a test instead of by a derive.
struct Route {
    method: &'static str,
    /// As axum spells it, `:id` and all. The document converts it.
    path: &'static str,
    summary: &'static str,
    /// What a caller has to have. Free-text, and the same words the guide uses.
    needs: &'static str,
}

const ROUTES: &[Route] = &[
    Route {
        method: "get",
        path: "/v1/health",
        summary: "Liveness, and the version answering.",
        needs: "nothing",
    },
    Route {
        method: "get",
        path: "/v1/session",
        summary: "Who is signed in, on what plan, with which property and palette.",
        needs: "token",
    },
    Route {
        method: "post",
        path: "/v1/session",
        summary: "Start the Google sign-in; opens a browser and answers at once.",
        needs: "token",
    },
    Route {
        method: "delete",
        path: "/v1/session",
        summary: "Revoke the credentials and forget them.",
        needs: "token",
    },
    Route {
        method: "get",
        path: "/v1/subscription",
        summary: "Plan, status, and the prices on offer.",
        needs: "token",
    },
    Route {
        method: "post",
        path: "/v1/subscription/checkout",
        summary: "A Stripe checkout URL, already tied to this account.",
        needs: "account",
    },
    Route {
        method: "get",
        path: "/v1/properties",
        summary: "Every GA4 property this login can see.",
        needs: "plan",
    },
    Route {
        method: "post",
        path: "/v1/properties",
        summary: "Create a property and its web stream; returns the tag.",
        needs: "plan",
    },
    Route {
        method: "delete",
        path: "/v1/properties/:id",
        summary: "Move a property to the Analytics trash. Wants the id twice.",
        needs: "plan",
    },
    Route {
        method: "get",
        path: "/v1/properties/:id/streams",
        summary: "The web data streams on a property, with their ids.",
        needs: "plan",
    },
    Route {
        method: "post",
        path: "/v1/properties/:id/streams",
        summary: "Add a web stream to a property that has none; returns the tag.",
        needs: "plan",
    },
    Route {
        method: "put",
        path: "/v1/property",
        summary: "Set the default property, the way `craft use` does.",
        needs: "plan",
    },
    Route {
        method: "get",
        path: "/v1/tag/:measurement_id",
        summary: "The snippet and the agent prompt for an id you already have.",
        needs: "token",
    },
    Route {
        method: "get",
        path: "/v1/themes",
        summary: "The palettes the dashboard ships, and which is in force.",
        needs: "token",
    },
    Route {
        method: "put",
        path: "/v1/themes",
        summary: "Wear one, the way `craft theme` does.",
        needs: "token",
    },
    Route {
        method: "get",
        path: "/v1/overview",
        summary: "Users, sessions, views, key events, bounce, duration, with deltas.",
        needs: "plan",
    },
    Route {
        method: "get",
        path: "/v1/pages",
        summary: "Most-visited pages.",
        needs: "plan",
    },
    Route {
        method: "get",
        path: "/v1/events",
        summary: "Events by count, against the previous period.",
        needs: "plan",
    },
    Route {
        method: "get",
        path: "/v1/sources",
        summary: "Source / medium pairs by sessions.",
        needs: "plan",
    },
    Route {
        method: "get",
        path: "/v1/referrers",
        summary: "Referring pages by sessions.",
        needs: "plan",
    },
    Route {
        method: "get",
        path: "/v1/countries",
        summary: "Users by country.",
        needs: "plan",
    },
    Route {
        method: "get",
        path: "/v1/live",
        summary: "Who is on the site right now, by country.",
        needs: "plan",
    },
    Route {
        method: "post",
        path: "/v1/mcp",
        summary: "The Model Context Protocol, for a client that cannot spawn `craft mcp`.",
        needs: "token",
    },
    Route {
        method: "get",
        path: "/v1/audit",
        summary: "What is wrong with how the property measures. Reads only.",
        needs: "plan",
    },
    Route {
        method: "get",
        path: "/v1/openapi.json",
        summary: "This document.",
        needs: "nothing",
    },
];

/// The API as OpenAPI 3.1, built from [`ROUTES`] at request time.
///
/// Request time because of one field: `servers`. The port is whatever the OS
/// handed out this run, and a document that named the wrong one would send
/// every generated client to a closed door.
async fn openapi(State(app): State<Arc<App>>) -> Json<Value> {
    let mut paths = serde_json::Map::new();

    for route in ROUTES {
        // OpenAPI spells a parameter `{id}` where axum spells it `:id`.
        let mut path = String::new();
        let mut params = Vec::new();
        for segment in route.path.split('/').skip(1) {
            path.push('/');
            match segment.strip_prefix(':') {
                Some(name) => {
                    path.push_str(&format!("{{{name}}}"));
                    params.push(json!({
                        "name": name,
                        "in": "path",
                        "required": true,
                        "schema": { "type": "string" },
                    }));
                }
                None => path.push_str(segment),
            }
        }

        let mut operation = json!({
            "summary": route.summary,
            "description": format!("Needs: {}.", route.needs),
            "responses": {
                "200": {
                    "description": "The answer.",
                    "content": { "application/json": { "schema": { "type": "object" } } },
                },
                "4XX": {
                    "description": "A refusal, saying which kind and what to do about it.",
                    "content": { "application/json": { "schema": {
                        "type": "object",
                        "properties": { "error": { "type": "object", "properties": {
                            "code": { "type": "string" },
                            "message": { "type": "string" },
                            "checkout_url": { "type": "string" },
                        }}},
                    }}},
                },
            },
        });
        if !params.is_empty() {
            operation["parameters"] = json!(params);
        }
        if route.needs == "nothing" {
            // The one door that opens without the token, and the document has
            // to say so or a generated client will send one it does not have.
            operation["security"] = json!([]);
        }

        paths
            .entry(path)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("built as an object")
            .insert(route.method.to_string(), operation);
    }

    Json(json!({
        "openapi": "3.1.0",
        "info": {
            "title": "anacraft API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "craft serve: sign in with Google, register a GA4 tag, read the numbers. \
                            Loopback only. https://anacraft.dev/serve.html",
        },
        "servers": [{ "url": app.origin }],
        // Either one opens any of these doors. The query is spelled out
        // rather than left undocumented, because it is what the MCP link
        // uses and a generated client should know it is allowed.
        "security": [{ "bearer": [] }, { "query": [] }],
        "components": {
            "securitySchemes": {
                "bearer": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "The token `craft serve` printed when it started, \
                                    which is the same one it printed last time.",
                },
                "query": {
                    "type": "apiKey",
                    "in": "query",
                    "name": "token",
                    "description": "The same token, for a client that can be given \
                                    a URL and nothing else. Loopback only, and this \
                                    server keeps no log.",
                },
            },
        },
        "paths": paths,
    }))
}

// ------------------------------------------------------------- the guard ---

/// Bearer token, origin, and the idle clock, in front of everything but the
/// health check and the page.
async fn guard(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    app.last.store(now(), Ordering::Relaxed);

    // A browser tells us where it came from; anything that does not (curl, a
    // script) is not a browser being aimed at us by a page it is reading.
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if let Some(origin) = &origin {
        if !allowed(&app, origin) {
            return Fail::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                format!(
                    "{origin} is not an origin this server answers. It serves one page, at {}.",
                    app.origin
                ),
            )
            .into_response();
        }
    }

    // The preflight needs an answer before the token can be sent at all, so it
    // is settled here rather than behind the check below.
    if request.method() == axum::http::Method::OPTIONS {
        return cors(StatusCode::NO_CONTENT.into_response(), origin.as_deref());
    }

    let header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    // A client that can only be handed a URL has nowhere to put a header, and
    // that is most of the clients this endpoint exists for. See
    // [`Connector::link`] for why a query string is an acceptable place for
    // this particular token.
    let query = request.uri().query().and_then(token_in).unwrap_or_default();
    let presented = if header.is_empty() { &query } else { &header };
    let opened = app.opens(presented);
    if opened.is_none() {
        return cors(
            Fail::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "this server wants its token, and the request did not carry it. \
                 It is on the banner where `craft serve` is running, on that \
                 server's own page, and on `c` in `craft`."
                    .into(),
            )
            .into_response(),
            origin.as_deref(),
        );
    }

    // The token said which property its holder is asking about. Carried on
    // the request rather than read again in the handler, so there is one
    // place where a token becomes a property and it is the place that checked
    // the token.
    let mut request = request;
    if let Some(Opened::Property(id)) = opened {
        request.extensions_mut().insert(Asked(id));
    }

    cors(next.run(request).await, origin.as_deref())
}

/// The property a request's token named, put on the request by [`guard`].
#[derive(Clone)]
struct Asked(String);

fn allowed(app: &App, origin: &str) -> bool {
    // A hosted server has one public origin, spelled one way.
    if app.hosted.is_some() {
        return origin == app.origin;
    }
    // localhost and 127.0.0.1 are the same machine and different origins, so
    // both spellings of our own address are answered and nothing else is.
    origin == app.origin || origin == app.origin.replace("127.0.0.1", "localhost")
}

fn cors(mut response: Response, origin: Option<&str>) -> Response {
    let Some(origin) = origin.and_then(|o| HeaderValue::from_str(o).ok()) else {
        return response;
    };
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, content-type"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, DELETE, OPTIONS"),
    );
    response
}

/// Compared in constant time. The token is short-lived and local, but a
/// comparison that returns early is a comparison that can be measured, and
/// writing the four lines costs nothing.
/// The `token` parameter out of a query string, percent-decoded.
///
/// Hand-rolled rather than reached for: the router hands this middleware a
/// `Request`, not a typed `Query`, and one parameter out of one string is not
/// worth a second extractor on every route.
fn token_in(query: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == "token")
        .map(|(_, value)| unpercent(value))
}

/// Percent-decoding, the other half of [`license::encode`].
///
/// A malformed escape is left as the characters it is made of rather than
/// refused: this feeds a constant-time comparison against a token, and every
/// wrong answer lands in the same place.
fn unpercent(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&value[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ------------------------------------------------------------- refusals ---

struct Fail {
    status: StatusCode,
    code: &'static str,
    message: String,
    checkout: Option<String>,
}

type Answer = std::result::Result<Json<Value>, Fail>;

impl Fail {
    fn new(status: StatusCode, code: &'static str, message: String) -> Fail {
        Fail {
            status,
            code,
            message,
            checkout: None,
        }
    }

    fn bad(message: impl Into<String>) -> Fail {
        Fail::new(StatusCode::BAD_REQUEST, "bad_request", message.into())
    }

    fn cold() -> Fail {
        Fail::new(
            StatusCode::CONFLICT,
            "not_signed_in",
            "nothing is signed in — POST /v1/session opens the Google window.".into(),
        )
    }

    /// A plan is missing, and the answer carries where to get one. A 402 that
    /// only said no would leave every caller to hard-code a Stripe link.
    fn unpaid(plan: Tier, message: String) -> Fail {
        Fail {
            status: StatusCode::PAYMENT_REQUIRED,
            code: "payment_required",
            message,
            checkout: Some(crate::subscribe_url(plan).to_string()),
        }
    }
}

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        let mut error = json!({ "code": self.code, "message": self.message });
        if let Some(url) = self.checkout {
            error["checkout_url"] = json!(url);
        }
        (self.status, Json(json!({ "error": error }))).into_response()
    }
}

/// Everything below the HTTP layer reports with `anyhow`, and most of it is
/// Google saying no. The message is kept whole — `ga.rs` writes those, and
/// they already name the account permission that would fix it — and only the
/// status is decided here.
impl From<anyhow::Error> for Fail {
    fn from(err: anyhow::Error) -> Fail {
        let message = err
            .chain()
            .map(|cause| cause.to_string())
            .collect::<Vec<_>>()
            .join(": ");

        // Our own wording, from `ga::google_error` and `ClientCreds::load`, so
        // none of this is a guess at somebody else's string.
        let (status, code) = if message.contains("no OAuth client configured") {
            // Not Google refusing: this build has nothing to sign in with.
            // A 502 would send the caller looking for a fault at Google's end.
            (StatusCode::INTERNAL_SERVER_ERROR, "no_client")
        } else if message.contains("access denied") {
            (StatusCode::FORBIDDEN, "forbidden")
        } else if message.contains("rate-limited") {
            (StatusCode::TOO_MANY_REQUESTS, "rate_limited")
        } else {
            (StatusCode::BAD_GATEWAY, "google")
        };
        Fail::new(status, code, message)
    }
}

// ------------------------------------------------------------- the page ---

/// The reference at `/docs`. It ships with no routes in it and fills itself in
/// from `/v1/openapi.json`, so there is no second list of endpoints in this
/// binary to fall behind the first.
async fn api_page() -> Html<&'static str> {
    Html(API_PAGE)
}

async fn health(State(app): State<Arc<App>>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "mode": if app.hosted.is_some() { "hosted" } else { "local" },
    }))
}

// ----------------------------------------------------------- the session ---

async fn session(State(app): State<Arc<App>>) -> Answer {
    if app.demo {
        return Ok(Json(demo_session()));
    }
    let tokens = Tokens::load().map_err(Fail::from)?;
    let account = Auth::account().map_err(Fail::from)?;
    let cfg = Config::load().map_err(Fail::from)?;

    let state = match &*app.login.lock().expect("the login lock is never poisoned") {
        Login::Pending if tokens.is_none() => "pending",
        Login::Failed(_) if tokens.is_none() => "failed",
        _ => {
            if tokens.is_some() {
                "signed_in"
            } else {
                "idle"
            }
        }
    };
    let failure = match &*app.login.lock().expect("the login lock is never poisoned") {
        Login::Failed(why) if tokens.is_none() => Some(why.clone()),
        _ => None,
    };

    let mut out = json!({
        "signed_in": tokens.is_some(),
        "state": state,
        "demo": app.demo,
        "theme": cfg
            .theme
            .clone()
            .unwrap_or_else(|| crate::theme::palette().name.to_string()),
        "version": env!("CARGO_PKG_VERSION"),
    });
    if let Some(account) = account {
        out["email"] = json!(account.email);
        out["account_id"] = json!(account.sub);
    }
    if let Some(why) = failure {
        out["error"] = json!(why);
    }
    if let Some(property) = cfg.active_property() {
        out["property"] = json!({ "id": property.id, "name": property.display() });
    }

    // The plan, as this machine last heard it. Asked rather than cached here:
    // a page that is about to offer a checkout should not offer one to
    // somebody who paid on another laptop ten seconds ago.
    let tier = license::sync(&cfg).await;
    out["subscribed"] = json!(tier.is_some());
    out["tier"] = json!(tier.map(|t| t.name()));
    // What a page actually needs to know is not whether somebody pays for
    // something, but whether what they pay for opens this. The ladder stays
    // here rather than being reimplemented in JavaScript.
    out["entitled"] = json!(tier.is_some_and(|have| have.meets(PLAN)));
    out["plan"] = json!(PLAN.name());
    out["plan_price"] = json!(PLAN.monthly());
    Ok(Json(out))
}

/// Start the Google sign-in, and answer before it finishes.
///
/// The flow blocks on a loopback accept — it is waiting for a person to click
/// things — so it runs on a thread with a runtime of its own rather than
/// holding a worker of this one for the length of somebody's attention span.
async fn session_start(State(app): State<Arc<App>>) -> Answer {
    if app.demo {
        return Ok(Json(demo_session()));
    }
    if Tokens::load().map_err(Fail::from)?.is_some() {
        return Ok(Json(json!({ "state": "signed_in" })));
    }
    {
        let mut login = app.login.lock().expect("the login lock is never poisoned");
        if matches!(*login, Login::Pending) {
            return Ok(Json(json!({ "state": "pending" })));
        }
        *login = Login::Pending;
    }

    let state = app.clone();
    // Where Google's tab is sent afterwards: back here, token and all, so it
    // arrives on a working page rather than on a dead one telling it to go to
    // a terminal nobody was in. The tab that started the sign-in has been
    // polling all along and has moved on by itself; this is for the tab the
    // person is actually looking at.
    let back = format!("{}/#k={}", app.origin, app.token);
    std::thread::spawn(move || {
        let outcome = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("starting a runtime for the sign-in")
            .and_then(|rt| {
                rt.block_on(async {
                    let auth = Auth::new(reqwest::Client::new())?;
                    auth.login_landing(&Landing {
                        title: "Signed in",
                        body: "anacraft has your Google account. \
                               Taking you to your properties…",
                        // Straight there, rather than a page with a button on
                        // it: this landing is the end of a step, not a choice.
                        redirect: Some(&back),
                        // Still carried, for the browser that will not follow
                        // a refresh — and it costs nothing in the one that
                        // does, which has already moved on.
                        cta: Some(Cta {
                            label: "Your properties →",
                            url: &back,
                            note: "if this page has not moved along by itself",
                        }),
                    })
                    .await?;
                    // Same courtesy `craft login` does: register the account
                    // so a subscription bought anywhere finds it here.
                    if let Some(account) = Auth::account()? {
                        let _ = license::link(&account).await;
                        let _ = license::sync(&Config::load()?).await;
                    }
                    Ok::<(), anyhow::Error>(())
                })
            });

        let mut login = state
            .login
            .lock()
            .expect("the login lock is never poisoned");
        *login = match outcome {
            Ok(()) => Login::Idle,
            Err(err) => Login::Failed(err.to_string()),
        };
    });

    Ok(Json(json!({ "state": "pending" })))
}

async fn session_end(State(app): State<Arc<App>>) -> Answer {
    if app.demo {
        return Ok(Json(demo_session()));
    }
    Auth::new(reqwest::Client::new())
        .map_err(Fail::from)?
        .logout()
        .await
        .map_err(Fail::from)?;
    let _ = license::forget();
    Ok(Json(json!({ "signed_in": false, "state": "idle" })))
}

// ------------------------------------------------------- the subscription ---

async fn subscription(State(app): State<Arc<App>>) -> Answer {
    if app.demo {
        return Ok(Json(
            json!({ "subscribed": true, "tier": "elite", "entitled": true, "demo": true, "plans": [] }),
        ));
    }
    let cfg = Config::load().map_err(Fail::from)?;
    let tier = license::sync(&cfg).await;
    Ok(Json(json!({
        "subscribed": tier.is_some(),
        "tier": tier.map(|t| t.name()),
        "plans": [
            { "plan": "basic", "price": Tier::Basic.monthly() },
            { "plan": "pro", "price": Tier::Pro.monthly() },
            { "plan": "elite", "price": Tier::Elite.monthly() },
        ],
    })))
}

#[derive(Deserialize)]
struct Checkout {
    plan: Option<String>,
}

/// A checkout URL already tied to the signed-in account, so the payment lands
/// on the row every `craft` command reads afterwards. The same two steps
/// `craft subscribe` takes: mint a token, tell the service whose it is.
async fn checkout(State(app): State<Arc<App>>, Json(body): Json<Checkout>) -> Answer {
    if app.demo {
        return Err(demo_only("starting a checkout"));
    }
    let plan = match body.plan.as_deref() {
        None => PLAN,
        Some(name) => {
            Tier::parse(name).ok_or_else(|| Fail::bad("plan has to be basic, pro or elite"))?
        }
    };
    let account = Auth::account()
        .map_err(Fail::from)?
        .ok_or_else(Fail::cold)?;

    let token = license::mint_token();
    // Best-effort, exactly as in the CLI: a claim that fails leaves the
    // checkout claimable by the email on it.
    let _ = license::claim(&token, &account).await;

    Ok(Json(json!({
        "url": license::checkout_url(crate::subscribe_url(plan), &token, account.email.as_deref()),
        "token": token,
        "plan": plan.name(),
    })))
}

// -------------------------------------------------------- the properties ---

async fn properties(State(app): State<Arc<App>>) -> Answer {
    if app.demo {
        return Ok(Json(json!({
            "properties": [{
                "id": "demo",
                "name": "Contoso Labs (demo)",
                "account": "Anacraft demo",
                "is_default": true,
            }],
            "note": "synthetic — run `craft serve` without --demo for the real ones",
        })));
    }
    let ga = client().await?;
    let cfg = Config::load().map_err(Fail::from)?;
    let props = ga.properties().await?;
    Ok(Json(json!({
        "properties": props.iter().map(|p| json!({
            "id": p.id,
            "name": p.name,
            "account": p.account,
            "is_default": cfg.active.as_deref() == Some(p.id.as_str()),
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct Register {
    url: String,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    currency: Option<String>,
}

/// The whole point of this server, in one call.
///
/// [`crate::configure::setup`] does the deciding — reuse what already measures
/// this host, finish a property that never got a stream, or create both — so
/// this is the same behaviour `craft configure` has, told in JSON. Consent is
/// `HeldOnly`: a request is not a place to open a browser and wait.
async fn register(State(app): State<Arc<App>>, Json(body): Json<Register>) -> Answer {
    let host = crate::configure::host_of(&body.url).map_err(|err| Fail::bad(err.to_string()))?;
    if app.demo {
        return Ok(Json(demo_tag(&host)?));
    }
    require("registering a tag").await?;

    let ga = client().await?;
    let setup = crate::configure::setup(
        &ga,
        &host,
        crate::configure::Options {
            account: body.account,
            timezone: body.timezone,
            currency: body.currency.unwrap_or_else(|| "USD".to_string()),
        },
        crate::configure::Consent::HeldOnly,
    )
    .await?;

    let mut out = tag_payload(&setup.stream.measurement_id);
    out["host"] = json!(host);
    out["action"] = json!(match setup.action {
        crate::configure::SetupAction::Reused => "reused",
        crate::configure::SetupAction::Finished => "finished",
        crate::configure::SetupAction::Created => "created",
    });
    out["property"] = json!({ "id": setup.property.id, "name": setup.property.name });
    out["default_uri"] = json!(setup.stream.default_uri);
    if let Some(timezone) = setup.timezone {
        out["timezone"] = json!(timezone);
    }
    if let Some(note) = setup.note {
        out["note"] = json!(note);
    }
    Ok(Json(out))
}

async fn streams(State(app): State<Arc<App>>, Path(id): Path<String>) -> Answer {
    if app.demo {
        return Ok(Json(json!({
            "property": bare(&id),
            "streams": [{
                "measurement_id": "G-DEMO1A2B3C4D",
                "url": "https://contoso.example",
                "name": "contoso.example",
            }],
        })));
    }
    let ga = client().await?;
    let property = bare(&id);
    let found = ga.web_streams(&property).await?;
    Ok(Json(json!({
        "property": property,
        "streams": found.iter().map(|s| json!({
            "measurement_id": s.measurement_id,
            "url": s.default_uri,
            "name": s.name,
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct Stream {
    url: String,
}

async fn add_stream(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(body): Json<Stream>,
) -> Answer {
    let host = crate::configure::host_of(&body.url).map_err(|err| Fail::bad(err.to_string()))?;
    if app.demo {
        return Ok(Json(demo_tag(&host)?));
    }
    require("adding a web stream").await?;

    let ga = client().await?;
    let property = bare(&id);
    let stream = ga
        .create_web_stream(&property, &host, &format!("https://{host}"))
        .await?;

    let mut out = tag_payload(&stream.measurement_id);
    out["host"] = json!(host);
    out["property"] = json!({ "id": property });
    out["default_uri"] = json!(stream.default_uri);
    Ok(Json(out))
}

#[derive(Deserialize)]
struct Confirm {
    confirm: Option<String>,
}

/// Move a property to Google's trash — the one destructive call in this API,
/// and the same one `craft delete --all` makes.
///
/// What keeps it defensible is that Google's delete is a soft one: the
/// property sits in the account's trash for 35 days, fully restorable from the
/// console, before anything is actually gone. The undo is Google's, it is
/// where a person would look for it, and nothing here can shorten it.
///
/// It is opt-in twice, the way the command is. The command needs a subcommand
/// and then a flag; this needs the id in the path and the same id again in
/// `?confirm=`, so a `DELETE` aimed at the wrong row by a script that built
/// its URL wrong has to be wrong the same way twice.
async fn trash(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(query): Query<Confirm>,
) -> Answer {
    if app.demo {
        return Err(demo_only("deleting a property"));
    }
    let property = bare(&id);
    if query.confirm.as_deref().map(bare).as_deref() != Some(property.as_str()) {
        return Err(Fail::bad(format!(
            "deleting is confirmed by saying the id twice — DELETE /v1/properties/{property}?confirm={property}"
        )));
    }

    let ga = client().await?;
    ga.delete_property(&property).await?;

    // Google first, then here — the same order the command uses, because
    // forgetting is local and reversible and a failed API call is not a
    // reason to have already pointed the dashboard away from a property that
    // is still sitting there collecting.
    let mut cfg = Config::load().map_err(Fail::from)?;
    let forgotten = cfg.remove(&property);
    if forgotten {
        cfg.save().map_err(Fail::from)?;
    }

    Ok(Json(json!({
        "property": property,
        "state": "trashed",
        "forgotten": forgotten,
        "restorable_days": 35,
        "note": "moved to the Analytics trash — it has stopped collecting, and Google keeps it \
                 restorable from the console for 35 days",
    })))
}

#[derive(Deserialize)]
struct Chosen {
    id: String,
}

/// `craft use`, over HTTP. It checks the property is one this login can see
/// before writing it down, for the same reason the command does: a default
/// nothing can read is a dashboard that opens on an error.
async fn use_property(State(app): State<Arc<App>>, Json(body): Json<Chosen>) -> Answer {
    if app.demo {
        return Err(demo_only("saving a default property"));
    }
    let ga = client().await?;
    let wanted = bare(&body.id);
    let props = ga.properties().await?;
    let found = props.iter().find(|p| p.id == wanted).ok_or_else(|| {
        Fail::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("no property {wanted} on this account"),
        )
    })?;

    let mut cfg = Config::load().map_err(Fail::from)?;
    cfg.upsert(&found.id, Some(found.name.clone()));
    cfg.save().map_err(Fail::from)?;
    Ok(Json(
        json!({ "property": { "id": found.id, "name": found.name } }),
    ))
}

async fn tag(Path(measurement_id): Path<String>) -> Answer {
    let id = measurement_id.trim().to_uppercase();
    if !id.starts_with("G-") || id.len() < 4 {
        return Err(Fail::bad("a measurement id looks like G-XXXXXXXXXX"));
    }
    Ok(Json(tag_payload(&id)))
}

/// The two blocks of text somebody actually leaves with. Both are the
/// binary's, so the terminal, the MCP tool and this server cannot drift into
/// handing out three slightly different tags.
fn tag_payload(measurement_id: &str) -> Value {
    json!({
        "measurement_id": measurement_id,
        "tag": crate::configure::tag_snippet(measurement_id),
        "prompt": crate::configure::tag_prompt(measurement_id),
    })
}

// ------------------------------------------------------------ the palette ---

/// The palettes the dashboard ships, and which one is in force.
///
/// The page wears whichever the CLI is set to, rather than keeping a
/// preference of its own. The config file is the one place a choice
/// survives, and it is the same line `craft theme` writes and the dashboard
/// reads.
async fn themes() -> Answer {
    let cfg = Config::load().map_err(Fail::from)?;
    let current = cfg
        .theme
        .clone()
        .unwrap_or_else(|| crate::theme::palette().name.to_string());
    Ok(Json(json!({
        "current": current,
        "themes": crate::theme::THEMES.iter().map(|p| json!({
            "name": p.name,
            "current": p.name == current,
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct Theme {
    name: String,
}

async fn use_theme(State(app): State<Arc<App>>, Json(body): Json<Theme>) -> Answer {
    if !crate::theme::select(&body.name) {
        return Err(Fail::bad(format!(
            "no theme called {} — GET /v1/themes lists them",
            body.name
        )));
    }
    // A demo changes nothing on this machine, and a line in the config file is
    // something on this machine. The page still restyles itself; it just does
    // not outlive the run.
    if app.demo {
        return Ok(Json(json!({ "theme": body.name, "saved": false })));
    }
    let mut cfg = Config::load().map_err(Fail::from)?;
    cfg.theme = Some(body.name.clone());
    cfg.save().map_err(Fail::from)?;
    Ok(Json(json!({ "theme": body.name, "saved": true })))
}

// ----------------------------------------------------------- the reports ---

#[derive(Deserialize)]
struct Read {
    property: Option<String>,
    days: Option<u32>,
    limit: Option<i64>,
    q: Option<String>,
}

async fn overview(State(app): State<Arc<App>>, Query(read): Query<Read>) -> Answer {
    report(&app, "site_status", read).await
}
async fn pages(State(app): State<Arc<App>>, Query(read): Query<Read>) -> Answer {
    let tool = if read.q.is_some() {
        "search_pages"
    } else {
        "list_pages"
    };
    report(&app, tool, read).await
}
async fn events(State(app): State<Arc<App>>, Query(read): Query<Read>) -> Answer {
    let tool = if read.q.is_some() {
        "search_events"
    } else {
        "list_events"
    };
    report(&app, tool, read).await
}
async fn sources(State(app): State<Arc<App>>, Query(read): Query<Read>) -> Answer {
    report(&app, "list_traffic_sources", read).await
}
async fn referrers(State(app): State<Arc<App>>, Query(read): Query<Read>) -> Answer {
    report(&app, "list_referrers", read).await
}
async fn countries(State(app): State<Arc<App>>, Query(read): Query<Read>) -> Answer {
    report(&app, "list_countries", read).await
}
async fn live(State(app): State<Arc<App>>, Query(read): Query<Read>) -> Answer {
    report(&app, "live_visitors", read).await
}

async fn audit(State(app): State<Arc<App>>, Query(read): Query<Read>) -> Answer {
    report(&app, "audit_site", read).await
}

/// One shape for every read: resolve the property, run the report the MCP
/// server would have run for the same question, and wrap it in the envelope
/// that says which property and which window the numbers are of.
async fn report(app: &App, tool: &str, read: Read) -> Answer {
    let days = read.days.unwrap_or(crate::DEFAULT_DAYS).clamp(1, 365);
    let limit = read
        .limit
        .unwrap_or(crate::DEFAULT_LIMIT as i64)
        .clamp(1, 100);
    let window = if tool == "live_visitors" {
        None
    } else {
        Some(days)
    };

    if app.demo {
        let args = json!({ "days": days, "limit": limit, "query": read.q });
        return Ok(Json(crate::mcp::demo::tool(tool, &args, days, limit)?));
    }

    let ga = client().await?;
    let cfg = Config::load().map_err(Fail::from)?;
    let property = cfg
        .resolve_property(read.property.as_deref().or(app.property.as_deref()))
        .map_err(|err| Fail::bad(err.to_string()))?;
    let named = cfg.find(&property).map(|p| p.display());

    let payload = match tool {
        "site_status" => crate::mcp::site_status(&ga, &property, days).await?,
        "live_visitors" => crate::mcp::live_visitors(&ga, &property).await?,
        "list_events" => crate::mcp::list_events(&ga, &property, days, limit).await?,
        "audit_site" => {
            crate::audit::inspect(&ga, &property, named.as_deref().unwrap_or(""), days).await?
        }
        "list_pages" | "search_pages" => {
            crate::mcp::ranked(
                &ga,
                &property,
                days,
                limit,
                "pagePath",
                "screenPageViews",
                read.q.as_deref(),
            )
            .await?
        }
        "search_events" => {
            crate::mcp::ranked(
                &ga,
                &property,
                days,
                limit,
                "eventName",
                "eventCount",
                read.q.as_deref(),
            )
            .await?
        }
        "list_referrers" => {
            crate::mcp::ranked(
                &ga,
                &property,
                days,
                limit,
                "pageReferrer",
                "sessions",
                None,
            )
            .await?
        }
        "list_traffic_sources" => {
            crate::mcp::ranked(
                &ga,
                &property,
                days,
                limit,
                "sessionSourceMedium",
                "sessions",
                None,
            )
            .await?
        }
        "list_countries" => {
            crate::mcp::ranked(&ga, &property, days, limit, "country", "totalUsers", None).await?
        }
        other => return Err(Fail::bad(format!("no report called {other}"))),
    };

    Ok(Json(crate::mcp::envelope(
        &property,
        named.as_deref(),
        window,
        payload,
    )))
}

// --------------------------------------------------------------- the demo ---

/// `--demo` is the whole flow with the account taken out, so the page can be
/// walked end to end — and looked at, and tested — on a machine that has never
/// signed in. The reports already have synthetic answers in [`crate::mcp`];
/// these are the rest of them.
///
/// What it must never do is write. A demo that quietly created a property in
/// somebody's real Analytics account because credentials happened to be lying
/// on the disk would be the worst kind of surprise, so every write either
/// answers synthetically or refuses by name.
fn demo_session() -> Value {
    json!({
        "signed_in": true,
        "state": "signed_in",
        "demo": true,
        "subscribed": true,
        "tier": "elite",
        "entitled": true,
        "theme": crate::theme::palette().name,
        "version": env!("CARGO_PKG_VERSION"),
        "property": { "id": "demo", "name": "Contoso Labs (demo)" },
        "note": "synthetic — run `craft serve` without --demo to use a real account",
    })
}

fn demo_tag(host: &str) -> std::result::Result<Value, Fail> {
    let mut out = tag_payload("G-DEMO1A2B3C4D");
    out["host"] = json!(host);
    out["action"] = json!("created");
    out["property"] = json!({ "id": "3900000000", "name": host });
    out["default_uri"] = json!(format!("https://{host}"));
    out["synthetic"] = json!(true);
    out["note"] =
        json!("synthetic — nothing was created. Restart without --demo to make the real property.");
    Ok(out)
}

fn demo_only(what: &str) -> Fail {
    Fail::new(
        StatusCode::CONFLICT,
        "not_supported",
        format!(
            "this server is running with --demo, which changes nothing on this machine \
             or in any Analytics account, so {what} is not something it can do."
        ),
    )
}

// --------------------------------------------------------------- the mcp ---

/// `POST /v1/mcp` — the Model Context Protocol, for a client that cannot spawn
/// `craft mcp` as a child process.
///
/// That client is not hypothetical. A strictly confined snap cannot read a
/// top-level hidden directory in `$HOME`, which is both where the binary
/// usually sits and where `~/.anacraft/` keeps the credentials; a Mac App
/// Store build has no equivalent of even that much; a container may have no
/// home directory worth the name. All three can open a loopback socket. So the
/// process stays out here, where the credentials are readable, and the client
/// is handed a URL and the bearer token instead of a command line.
///
/// This is the Streamable HTTP transport in the shape a server with nothing to
/// say unprompted is allowed to take: every answer is an immediate
/// `application/json` body, there is no event stream and no session id, and
/// the `GET` that would open one is refused below rather than half-kept.
///
/// The token and the `Origin` check are [`guard`]'s, already settled before
/// this runs — the latter being exactly what the specification asks of an HTTP
/// transport on loopback, and the reason a page on the public web cannot reach
/// this door.
///
/// Nothing here calls [`require`]. The plan and the login are the MCP server's
/// own business and it answers a missing one as a tool error carrying the
/// sentence that fixes it, which an assistant can relay. A 402 from this layer
/// would reach the user as a connector that is simply broken.
async fn mcp(
    State(app): State<Arc<App>>,
    asked: Option<axum::Extension<Asked>>,
    body: axum::body::Bytes,
) -> Response {
    let message: Value = match serde_json::from_slice(&body) {
        Ok(message) => message,
        // The answer the stdio pump gives, for the same reason: bad JSON is a
        // JSON-RPC error, and dressing it as a 400 would tell the client its
        // transport is broken when its message was.
        Err(err) => {
            return Json(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": format!("invalid JSON: {err}") },
            }))
            .into_response()
        }
    };

    // The property the token named, else `--property`, else whatever the
    // config has. A connector is wired up against one property and should go
    // on reading that one — an assistant that started answering about another
    // site because somebody pressed `tab` in the dashboard is worse than one
    // that stopped.
    let want = asked
        .map(|axum::Extension(Asked(id))| id)
        .or_else(|| app.property.clone())
        .unwrap_or_default();

    let mut slot = app.mcp.lock().await;

    // Built on the first call, and built again while it is locked. The reason
    // it is locked can go away underneath us: `craft serve` opens the page
    // where somebody signs in or subscribes, and a server built one request
    // earlier would otherwise answer "not logged in" for the rest of the run.
    // Rebuilt too when the property changed, which is the other connector
    // asking. Once it is serving numbers this costs two comparisons.
    if slot.as_ref().map_or(true, |(built, server)| {
        built != &want || server.lock_reason().is_some()
    }) {
        match crate::mcp::build(app.demo, Some(want.as_str()).filter(|id| !id.is_empty())).await {
            Ok(server) => *slot = Some((want.clone(), server)),
            Err(err) => return Fail::from(err).into_response(),
        }
    }
    let (_, server) = slot.as_mut().expect("just built or already there");
    answer(server, message).await
}

/// One JSON-RPC message, or a batch of them, answered by `server` — the half
/// of `/v1/mcp` the local and the hosted server share.
async fn answer(server: &mut crate::mcp::Server, message: Value) -> Response {
    // A batch is a 2025-03-26 spelling that 2025-06-18 withdrew. Answering one
    // is a loop, and it saves a client on the older revision from getting
    // silence back from a server that speaks its revision everywhere else.
    let reply = match message {
        Value::Array(messages) => {
            let mut replies = Vec::new();
            for message in messages {
                if let Some(reply) = server.dispatch(message).await {
                    replies.push(reply);
                }
            }
            (!replies.is_empty()).then_some(Value::Array(replies))
        }
        message => server.dispatch(message).await,
    };

    match reply {
        Some(reply) => Json(reply).into_response(),
        // Nothing to say, which is the right answer to a notification. The
        // specification spells it 202 with an empty body, and a `{}` here
        // would be a JSON-RPC message with no `id` for the client to match.
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// `GET /v1/mcp` — the event stream this server does not have.
///
/// The transport lets a client open an SSE channel for messages the server
/// starts on its own. Nothing here ever does: every tool is one question and
/// one answer, with no subscriptions, no progress and no sampling. Saying so
/// is better than holding a socket open against traffic that is never coming,
/// and a client that reads this falls back to POST, which is all it needed.
async fn mcp_no_stream() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST")],
        Json(json!({ "error": {
            "code": "no_stream",
            "message": "this server never speaks first, so there is no stream to open \u{2014} \
                        POST a JSON-RPC message instead.",
        }})),
    )
        .into_response()
}

// ------------------------------------------------------------- the small ---

/// A client, or the one refusal that is not Google's fault: nobody is signed
/// in yet.
async fn client() -> std::result::Result<Ga, Fail> {
    client_in(&Ctx::Local).await
}

/// The same, for whoever the request is from: this machine, or one hosted
/// visitor.
async fn client_in(ctx: &Ctx) -> std::result::Result<Ga, Fail> {
    if !ctx.has_tokens().map_err(Fail::from)? {
        return Err(Fail::cold());
    }
    // The plan is checked here rather than at each call site, because here is
    // where a real Analytics account is about to be reached, and every route
    // that reaches one comes through this function.
    require_in(ctx, "the API").await?;
    ctx.ga().map_err(Fail::from)
}

/// The plan this machine is on, against the plan a call needs. The refusal
/// text is [`crate::license::gate`]'s, so a caller reads the same sentence the
/// terminal prints.
async fn require(what: &str) -> std::result::Result<(), Fail> {
    require_in(&Ctx::Local, what).await
}

async fn require_in(ctx: &Ctx, what: &str) -> std::result::Result<(), Fail> {
    let have = ctx.tier().await.map_err(Fail::from)?;
    license::gate(have, PLAN, what).map_err(|message| Fail::unpaid(PLAN, message))
}

/// `properties/397412345` and `397412345` are the same property, and a caller
/// should not have to know which spelling we wanted.
fn bare(id: &str) -> String {
    id.trim().trim_start_matches("properties/").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_compared_whole() {
        let token = license::mint_token();
        assert!(same(&token, &token));
        assert!(!same(&token, &token[..token.len() - 1]));
        assert!(!same("", &token));

        // A prefix that matches must not pass, which is the thing the
        // length check above is really guarding.
        let mut nearly = token.clone();
        nearly.pop();
        nearly.push(if token.ends_with('a') { 'b' } else { 'a' });
        assert!(!same(&nearly, &token));
    }

    #[test]
    fn only_this_servers_own_origin_is_answered() {
        let app = App {
            token: "t".into(),
            keys: Keys::Named("t".into()),
            origin: "http://127.0.0.1:52413".into(),
            port: 52413,
            demo: false,
            property: None,
            login: Mutex::new(Login::Idle),
            last: AtomicU64::new(0),
            mcp: tokio::sync::Mutex::new(None),
            hosted: None,
        };
        assert!(allowed(&app, "http://127.0.0.1:52413"));
        assert!(allowed(&app, "http://localhost:52413"));
        // A different port is a different program on the same machine, and
        // https://anacraft.dev is not this machine at all.
        assert!(!allowed(&app, "http://127.0.0.1:3000"));
        assert!(!allowed(&app, "https://anacraft.dev"));
        assert!(!allowed(&app, "null"));
    }

    #[test]
    fn the_port_last_served_from_is_the_one_tried_next() {
        // Nothing remembered, nothing asked: the OS chooses, as it always did.
        assert_eq!(wanted_port(0, None), 0);
        // Remembered and not overridden — the case a connector depends on.
        assert_eq!(wanted_port(0, Some(7777)), 7777);
        // `--port` is the caller saying which, and outranks the memory.
        assert_eq!(wanted_port(7788, Some(7777)), 7788);
        assert_eq!(wanted_port(7788, None), 7788);
    }

    #[test]
    fn a_port_taken_around_a_squatter_is_not_the_one_remembered() {
        // Two servers, and the second one cannot have the port the first is
        // on. Writing the port it settled for into the file would move the
        // connectors that point at the first — so the memory is only written
        // when the port was this run's to choose.
        //
        // The flag on `run` is the whole of the rule; this is it stated
        // against the two cases that reach it.
        let remembered = Some(39999u16);
        // Nothing asked, something remembered: that is the port to try, and
        // failing to get it is the case that must not be written down.
        assert_eq!(wanted_port(0, remembered), 39999);
        // Nothing asked, nothing remembered: whatever the OS gives is this
        // run's own choice, and worth remembering.
        assert_eq!(wanted_port(0, None), 0);
    }

    #[test]
    fn a_half_written_endpoint_remembers_nothing_rather_than_failing() {
        // The file is 0600 next to the credentials, but it is still a file on
        // somebody's disk: hand-edited, truncated, or written by a version
        // that had one field. None of that should stop the server starting,
        // so every unreadable shape has to land on the same answer as "no
        // file yet" — mint and bind afresh.
        let full: Endpoint =
            serde_json::from_str(r#"{"port":7777,"secret":"abc"}"#).expect("the whole pair");
        assert_eq!(
            (full.port, full.secret.as_deref()),
            (Some(7777), Some("abc"))
        );

        let partial: Endpoint = serde_json::from_str(r#"{"secret":"abc"}"#).expect("secret only");
        assert_eq!(wanted_port(0, partial.port), 0);

        // The shape this field replaced. A file left by the version that
        // remembered a minted token reads as a file with no secret in it,
        // which is the same as no file: one gets minted.
        let stale: Endpoint =
            serde_json::from_str(r#"{"port":7777,"token":"abc"}"#).expect("the old pair");
        assert_eq!(stale.secret, None);

        assert!(serde_json::from_str::<Endpoint>("{}").is_ok());
        assert!(serde_json::from_str::<Endpoint>("not json").is_err());
    }

    #[test]
    fn the_token_is_the_same_one_tomorrow() {
        // The property of the whole scheme: nothing here is random, so a
        // connector configured once stays configured.
        let a = derive("s3cret", "110147", "397412345");
        assert_eq!(a, derive("s3cret", "110147", "397412345"));
        // Forty characters of hex, which is the length and the alphabet the
        // minted token had — nothing downstream sees a new shape.
        assert_eq!(a.len(), 40);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_ids_alone_do_not_give_the_token() {
        // Why this is an HMAC and not a hash. A property id is printed in the
        // tag on every page of the site it measures; if knowing it and the
        // account were enough, the token would be public.
        let known = derive("secret-one", "110147", "397412345");
        assert_ne!(known, derive("secret-two", "110147", "397412345"));
        // And each id still moves it, so two properties are two tokens.
        assert_ne!(known, derive("secret-one", "110147", "397412346"));
        assert_ne!(known, derive("secret-one", "110148", "397412345"));
    }

    #[test]
    fn no_pair_of_ids_can_be_rearranged_into_another() {
        // Joined on a separator, account "1" with property "10:2" and account
        // "1:10" with property "2" would feed the same bytes in. Length
        // prefixes are what stop that being a second valid token.
        assert_ne!(derive("k", "1", "10:2"), derive("k", "1:10", "2"));
        assert_ne!(derive("k", "", "a"), derive("k", "a", ""));
    }

    #[test]
    fn a_property_switched_away_from_still_opens_the_door() {
        // The failure this guards against: a connector wired up against the
        // property that was active yesterday, and a dashboard `tab` press
        // since. It must not start refusing.
        let ring = keyring(
            "k",
            "110147",
            "397412345",
            &["397412345".into(), "88".into()],
        );
        assert_eq!(ring.len(), 2);
        // The active one is what gets advertised.
        assert_eq!(
            ring[0],
            ("397412345".into(), derive("k", "110147", "397412345"))
        );
        assert!(ring.contains(&("88".into(), derive("k", "110147", "88"))));

        // Switched: the other property is advertised now, and yesterday's
        // token is still on the ring, still naming the property it was cut
        // for.
        let after = keyring("k", "110147", "88", &["397412345".into(), "88".into()]);
        assert_eq!(after[0].0, "88");
        assert!(after.contains(&("397412345".into(), derive("k", "110147", "397412345"))));
    }

    #[test]
    fn a_token_names_the_property_it_reads() {
        // The whole reason the ring is pairs. A connector copied from one row
        // of the list must go on reading that row's property, whatever the
        // dashboard is showing — otherwise copying per property is a lie.
        let ring = keyring(
            "k",
            "110147",
            "397412345",
            &["397412345".into(), "88".into()],
        );
        for (id, token) in &ring {
            match opened(&ring, token) {
                Some(Opened::Property(named)) => assert_eq!(&named, id),
                _ => panic!("{id}'s own token did not open it"),
            }
        }
        assert!(opened(&ring, "not a token of this ring").is_none());
    }

    #[test]
    fn a_named_token_names_no_property() {
        // `--token` is a caller saying which token, not which property, so
        // the reads fall back to the flag and the config the way they did
        // before any of this was derived.
        let ring = Keys::Named("given".into()).ring(None);
        assert!(matches!(opened(&ring, "given"), Some(Opened::Server)));
        assert!(opened(&ring, "guessed").is_none());
    }

    #[test]
    fn a_keyring_is_never_empty() {
        // Nobody signed in, nothing configured — the first run, which is the
        // run that opens the page where signing in happens. There is still a
        // token, and it is still unguessable, because the secret is what was
        // doing that work all along.
        let ring = keyring("k", "", "", &[]);
        assert_eq!(ring, vec![(String::new(), derive("k", "", ""))]);
    }

    #[test]
    fn the_demo_hands_out_the_token_it_actually_answers_to() {
        // The demo's property id is not in anybody's config, so a token
        // derived for it would be one this server has never heard of — a page
        // handing out a key to its own front door that does not turn.
        let app = App {
            token: "banner".into(),
            keys: Keys::Derived {
                secret: "k".into(),
                account: "110147".into(),
            },
            origin: "http://127.0.0.1:52413".into(),
            port: 52413,
            demo: true,
            property: None,
            login: Mutex::new(Login::Idle),
            last: AtomicU64::new(0),
            mcp: tokio::sync::Mutex::new(None),
            hosted: None,
        };
        assert_eq!(app.token_for("demo"), "banner");
        assert_eq!(app.token_for("397412345"), "banner");
        assert!(app.one_token());
    }

    #[test]
    fn the_link_carries_both_halves() {
        // What somebody pastes into a client that has room for a URL and
        // nothing else. If the token is not in it, it is a link that fails
        // after they have stopped looking.
        let wire = Connector::new(7777, "abc123".into());
        assert_eq!(wire.url, "http://127.0.0.1:7777/v1/mcp");
        assert_eq!(wire.link, "http://127.0.0.1:7777/v1/mcp?token=abc123");
        assert!(wire.command.contains(&wire.link));
        // Quoted in the command, because `?` is a glob to every shell that
        // will see it.
        assert!(wire.command.contains(&format!("'{}'", wire.link)));
        // One line each: they go through a clipboard into a shell.
        assert!(!wire.command.contains('\n'));
        assert!(!wire.link.contains('\n'));
    }

    #[test]
    fn a_token_with_punctuation_in_it_survives_the_query() {
        // `--token` takes anything, and a `&` or a `?` in one would otherwise
        // end the parameter early and hand back a token that is not the one.
        let awkward = "a&b?c=d e/f%g";
        let wire = Connector::new(7777, awkward.into());
        let query = wire.link.split_once('?').expect("a query").1;
        assert_eq!(token_in(query).as_deref(), Some(awkward));
    }

    #[test]
    fn the_query_gives_up_the_token_and_nothing_else() {
        assert_eq!(token_in("token=abc").as_deref(), Some("abc"));
        // Beside other parameters, in either order.
        assert_eq!(token_in("x=1&token=abc").as_deref(), Some("abc"));
        assert_eq!(token_in("token=abc&x=1").as_deref(), Some("abc"));
        // Not a parameter that merely ends in the word.
        assert_eq!(token_in("mytoken=abc"), None);
        assert_eq!(token_in("x=1"), None);
        assert_eq!(token_in(""), None);
    }

    #[test]
    fn a_malformed_escape_is_left_alone_rather_than_refused() {
        // It is about to be compared against a token, and every wrong answer
        // lands in the same place — so decoding has no reason to have a
        // failure case of its own.
        assert_eq!(unpercent("ab%"), "ab%");
        assert_eq!(unpercent("ab%zz"), "ab%zz");
        assert_eq!(unpercent("a%20b"), "a b");
        assert_eq!(unpercent("a+b"), "a b");
    }

    #[test]
    fn a_property_id_is_taken_in_either_spelling() {
        assert_eq!(bare("properties/397412345"), "397412345");
        assert_eq!(bare(" 397412345 "), "397412345");
    }

    #[test]
    fn deleting_asks_for_the_id_twice() {
        // The check itself is in the handler, which needs a server to call.
        // What is testable without one is the pair it compares — and that both
        // spellings of a property id land on the same string, so a `confirm`
        // written as `properties/1` still matches a path of `1`.
        assert_eq!(bare("properties/397412345"), bare("397412345"));
        assert_ne!(bare("397412345"), bare("397412346"));
    }

    #[test]
    fn the_tag_payload_carries_both_blocks_with_the_id_in_them() {
        let payload = tag_payload("G-1A2BCD345E");
        let tag = payload["tag"].as_str().unwrap();
        let prompt = payload["prompt"].as_str().unwrap();

        // Both places the id belongs, in the block, and the block inside the
        // prompt — the prompt is the tag plus instructions, never a retyping
        // of it.
        assert_eq!(tag.matches("G-1A2BCD345E").count(), 2);
        assert!(prompt.contains(tag));
    }

    #[test]
    fn the_reference_fetches_nothing_from_anywhere_else() {
        // `craft serve` runs on machines behind corporate proxies, on
        // aeroplanes, and on a laptop whose only working connection is the one
        // to Google. A stylesheet or a font from a CDN would turn the page
        // that hands over a measurement id into a page that sometimes renders.
        // The same test for the views is in `views.rs`, where a rendered page
        // can be looked at rather than a template.
        for tag in [
            "<script src",
            "<link",
            "@import",
            "fonts.googleapis",
            "cdn.",
        ] {
            assert!(
                !API_PAGE.contains(tag),
                "assets/api.html reaches for `{tag}` — this page has to be self-contained"
            );
        }
        // The only outbound address the markup points at is a link a person
        // clicks, never something the browser fetches on load. (The other
        // `https://` strings in the file are a placeholder and an error
        // message, which ask nothing of the network.)
        assert!(API_PAGE.contains("href=\"https://anacraft.dev/serve.html\""));
        assert!(!API_PAGE.contains("src=\"http"));
    }

    /// This module's own source, read at compile time, so the router can be
    /// checked against the table that documents it. The same trick `ga.rs`
    /// uses to keep the scope submission honest, and for the same reason:
    /// nothing else notices when code and documentation part company.
    const SOURCE: &str = include_str!("serve.rs");

    /// Every path handed to `route(`, above the test module.
    ///
    /// Comment lines are dropped before the scan, which is not fussiness: the
    /// doc comment on `ROUTES` mentions the call by name, and without this the
    /// table would be checked against a sentence about itself.
    fn routed() -> Vec<String> {
        let code: String = SOURCE
            .split_once("\n#[cfg(test)]")
            .map(|(code, _)| code)
            .expect("this module is the first test module in the file")
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        code.match_indices(".route(")
            .map(|(at, _)| {
                // rustfmt puts a long route on its own line, so the path is
                // the first string literal after the paren rather than the
                // byte after it.
                let rest = &code[at + ".route(".len()..];
                let open = rest.find('"').expect("a route path is a string literal") + 1;
                let rest = &rest[open..];
                rest[..rest.find('"').expect("an unterminated route path")].to_string()
            })
            .collect()
    }

    /// Served, and deliberately not in the document: it answers HTML to a
    /// browser, and an OpenAPI path for it would describe the page as an
    /// endpoint that returns a string. The views have the same exemption and
    /// never reach this scan — they are routed in `views.rs`, and checked by
    /// the tests there.
    const NOT_API: [&str; 1] = ["/docs"];

    #[test]
    fn the_openapi_document_describes_exactly_what_is_routed() {
        for path in routed()
            .into_iter()
            .filter(|p| !NOT_API.contains(&p.as_str()))
        {
            assert!(
                ROUTES.iter().any(|r| r.path == path),
                "{path} is served and is not in ROUTES — the OpenAPI document would not mention it"
            );
        }
        for route in ROUTES {
            assert!(
                routed().iter().any(|p| p == route.path),
                "ROUTES documents {} and nothing routes it",
                route.path
            );
        }
    }

    #[test]
    fn a_refusal_for_a_missing_plan_says_where_to_get_one() {
        let fail = Fail::unpaid(Tier::Basic, "registering a tag needs a plan".into());
        assert_eq!(fail.status, StatusCode::PAYMENT_REQUIRED);
        assert!(fail
            .checkout
            .unwrap()
            .starts_with("https://buy.stripe.com/"));
    }
}
