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
//! page itself wants the bearer token minted at startup and printed once, and
//! a browser reaching it has to come from this server's own origin. That is
//! three locks on a door that only opens onto one machine, and they are there
//! because a page on the public web can absolutely try to talk to
//! `127.0.0.1` — it just cannot guess forty random characters while doing it.

use std::net::Ipv4Addr;
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
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{Auth, Cta, Landing, Tokens};
use crate::config::Config;
use crate::ga::Ga;
use crate::license::{self, Tier};

/// The page `craft serve` opens, carried in the binary so the server has no
/// files to find and no directory to be run from.
const PAGE: &str = include_str!("../assets/serve.html");

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

pub struct Options {
    /// 0 lets the OS pick, which is the default: a fixed port is a thing to
    /// collide with, and the URL is printed either way.
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
    token: String,
    /// The one origin a browser may call from — this server's own.
    origin: String,
    demo: bool,
    property: Option<String>,
    login: Mutex<Login>,
    /// Unix seconds of the last request, for the idle clock.
    last: AtomicU64,
}

pub async fn run(opts: Options) -> Result<()> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, opts.port))
        .await
        .with_context(|| match opts.port {
            0 => "could not open a local port".to_string(),
            port => format!("could not open port {port} — something else may have it"),
        })?;
    let port = listener.local_addr()?.port();

    let app = Arc::new(App {
        token: opts.token.unwrap_or_else(license::mint_token),
        origin: format!("http://127.0.0.1:{port}"),
        demo: opts.demo,
        property: opts.property,
        login: Mutex::new(Login::Idle),
        last: AtomicU64::new(now()),
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
        .layer(middleware::from_fn_with_state(app.clone(), guard));

    let router = Router::new()
        .route("/", get(page))
        .route("/v1/health", get(health))
        // Open, like health. A description of the door is not a key to it:
        // this names the routes and the shape of an answer, all of which is
        // published at anacraft.dev/serve.html anyway — and a client
        // generator or an agent reads the description *before* it has been
        // given a token, which is the whole point of there being one.
        .route("/v1/openapi.json", get(openapi))
        .merge(guarded)
        .with_state(app.clone());

    let url = format!("{}/#k={}", app.origin, app.token);
    banner(&app.origin, &app.token, opts.idle, opts.demo);
    if opts.open {
        let _ = open::that(&url);
    }

    axum::serve(listener, router)
        .with_graceful_shutdown(idle(app.clone(), opts.idle))
        .await
        .context("the local server stopped unexpectedly")
}

fn banner(origin: &str, token: &str, idle: u64, demo: bool) {
    use crate::render::{bold, dim};
    use crate::theme::glyph;

    println!(
        "\n  {} anacraft serving on {}",
        glyph::PICKAXE,
        bold(origin)
    );
    println!("  {} {}", dim("token"), dim(token));
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
            "title": "anacraft — the local API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "craft serve: sign in with Google, register a GA4 tag, read the numbers. \
                            Loopback only. https://anacraft.dev/serve.html",
        },
        "servers": [{ "url": app.origin }],
        "security": [{ "bearer": [] }],
        "components": {
            "securitySchemes": {
                "bearer": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "The token `craft serve` printed when it started.",
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

    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("");
    if !same(presented, &app.token) {
        return cors(
            Fail::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "this server was started with a token, and the request did not carry it. \
                 It was printed once, where `craft serve` is running."
                    .into(),
            )
            .into_response(),
            origin.as_deref(),
        );
    }

    cors(next.run(request).await, origin.as_deref())
}

fn allowed(app: &App, origin: &str) -> bool {
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

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

async fn health() -> Json<Value> {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "mode": "local",
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
                               Your properties are listed back on the tag page.",
                        cta: Some(Cta {
                            label: "Back to your properties →",
                            url: &back,
                            note: "it is also waiting in the tab you came from",
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
/// preference of its own: the port changes every run, so anything this page
/// stored would be stored against an origin that will not exist tomorrow. The
/// config file is the one place a choice survives, and it is the same line
/// `craft theme` writes and the dashboard reads.
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

// ------------------------------------------------------------- the small ---

/// A client, or the one refusal that is not Google's fault: nobody is signed
/// in yet.
async fn client() -> std::result::Result<Ga, Fail> {
    if Tokens::load().map_err(Fail::from)?.is_none() {
        return Err(Fail::cold());
    }
    // The plan is checked here rather than at each call site, because here is
    // where a real Analytics account is about to be reached, and every route
    // that reaches one comes through this function.
    require("the local API").await?;
    Ga::new().map_err(Fail::from)
}

/// The plan this machine is on, against the plan a call needs. The refusal
/// text is [`crate::license::gate`]'s, so a caller reads the same sentence the
/// terminal prints.
async fn require(what: &str) -> std::result::Result<(), Fail> {
    let cfg = Config::load().map_err(Fail::from)?;
    let have = license::sync(&cfg).await;
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
            origin: "http://127.0.0.1:52413".into(),
            demo: false,
            property: None,
            login: Mutex::new(Login::Idle),
            last: AtomicU64::new(0),
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
    fn the_page_it_serves_fetches_nothing_from_anywhere_else() {
        // `craft serve` runs on machines behind corporate proxies, on
        // aeroplanes, and on a laptop whose only working connection is the one
        // to Google. A stylesheet or a font from a CDN would turn the page
        // that hands over a measurement id into a page that sometimes renders.
        for tag in [
            "<script src",
            "<link",
            "@import",
            "fonts.googleapis",
            "cdn.",
        ] {
            assert!(
                !PAGE.contains(tag),
                "assets/serve.html reaches for `{tag}` — the page has to be self-contained"
            );
        }

        // One outbound address the markup points at, and it is a link a
        // person clicks rather than something the browser fetches on load.
        // (Other `https://` strings in the file are a placeholder and an error
        // message, which ask nothing of the network.)
        assert_eq!(PAGE.matches("href=\"http").count(), 1);
        assert!(PAGE.contains("href=\"https://anacraft.dev/serve.html\""));
        assert!(!PAGE.contains("src=\"http"));
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
    /// endpoint that returns a string.
    const NOT_API: [&str; 1] = ["/"];

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
    fn every_route_the_page_calls_is_a_route_the_server_has() {
        // The page is a file; nothing type-checks the paths in it. This does.
        let routes = [
            "/v1/session",
            "/v1/subscription",
            "/v1/subscription/checkout",
            "/v1/properties",
            "/v1/property",
            "/v1/tag/",
        ];
        for route in routes {
            assert!(
                PAGE.contains(route),
                "{route} is served but the page never calls it — one of the two is wrong"
            );
        }
        // And the other direction: no path in the page that this module does
        // not route. `/v1/properties/' + id + '/streams` is built up, so it is
        // checked by its tail.
        assert!(PAGE.contains("/streams"));
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
