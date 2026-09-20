//! The pages `craft serve` renders, one route at a time.
//!
//! This used to be a single HTML file with nine panes in it and a script that
//! showed one at a time. That works right up until somebody reloads, or
//! presses back, or wants to send a colleague the screen they are looking at
//! — at which point a flow with no URLs has nothing to offer any of them. So
//! the panes are views now: a route each, rendered here, in [`askama`]
//! templates under `templates/`.
//!
//! Nothing below decides anything. Every handler is the same work the JSON
//! API in the parent module does — [`crate::configure::setup`], the same
//! [`crate::license`] gate, the same [`crate::auth`] flow — with an `<h2>`
//! around the answer instead of a `Json`. The two are siblings over one set
//! of functions, not a page and a reimplementation of it.
//!
//! **The token.** The API takes it as a bearer header, which a `<form>`
//! cannot send. So the browser hands it over once — the fragment the terminal
//! printed, posted to `/session/key` by the only script on the way in — and
//! carries it in a cookie afterwards: `HttpOnly`, so no script can read it
//! back, and `SameSite=Strict`, so no other origin's form can spend it. The
//! origin check the API does is done here too, which leaves a cross-site POST
//! failing twice.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Form, FromRequest, Path, Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;

use super::{allowed, bare, client, now, require, App, Login, PLAN};
use crate::auth::{Auth, Cta, Landing, Tokens};
use crate::config::Config;
use crate::license::{self, Tier};

/// The stylesheet every view links, carried in the binary like the views are.
const STYLE: &str = include_str!("../../assets/serve.css");

/// The palette that is `:root` rather than a `[data-pal]` block — wearing it
/// means having no attribute at all, which is why there is no flash on the
/// way in.
const DEFAULT_PAL: &str = "osaka-jade";

/// The cookie the token lives in once the fragment has been handed over,
/// before the port is put on the end of it.
///
/// A cookie is scoped to a host and not to a port, so every `craft serve`
/// that has ever run on this machine shares one jar with every other. Under a
/// fixed name, yesterday's cookie arrives at today's server — which at best
/// fails the token check below, and at worst has two runs quietly
/// overwriting each other. The port makes each run's cookie its own.
const KEY: &str = "craft_key";

pub(super) fn router(app: Arc<App>) -> Router<Arc<App>> {
    // Everything that needs the token, which is everything but the way in and
    // the stylesheet.
    let guarded = Router::new()
        .route("/signin", get(signin).post(signin_start))
        .route("/signin/waiting", get(signin_waiting))
        .route("/signout", post(signout))
        .route("/unlock", get(unlock))
        .route("/unlock/checkout", post(unlock_checkout))
        .route("/properties", get(properties).post(create))
        .route("/properties/new", get(new_property))
        .route("/properties/:id/trash", get(trash_ask).post(trash_do))
        .route("/properties/:id/streams", get(streams).post(add_stream))
        .route("/property", post(use_property))
        .route("/themes", post(use_theme))
        .route("/tag/:measurement_id", get(tag))
        .layer(middleware::from_fn_with_state(app, keyed));

    Router::new()
        .route("/", get(landing))
        .route("/app.css", get(stylesheet))
        .route("/session/key", post(hand_over))
        .merge(guarded)
}

// --------------------------------------------------------------- the shell ---

pub(super) struct Chip {
    name: &'static str,
    current: bool,
}

/// What the layout needs whatever the view is: the palette in force, who is
/// signed in, which colour the card's dot wears, and where the browser is
/// standing so a palette chip can send it back there.
pub(super) struct Shell {
    pal: String,
    who: String,
    state: &'static str,
    path: String,
    themes: Vec<Chip>,
}

impl Shell {
    /// Read rather than passed around: the palette is a process-wide choice
    /// and the account is a local file, and neither is worth threading
    /// through twelve handlers.
    ///
    /// The palette comes from [`crate::theme::palette`] rather than the
    /// config file, because the config file is only where it was last
    /// *saved*. `main` resolves `--theme`, the property's own palette and the
    /// saved default into that one selection before anything renders, and a
    /// chip clicked on this page moves it again — so this is the one reading
    /// that is right in all four cases.
    fn new(state: &'static str, path: impl Into<String>) -> Shell {
        let current = crate::theme::palette().name.to_string();
        Shell {
            pal: if current == DEFAULT_PAL {
                String::new()
            } else {
                current.clone()
            },
            who: String::new(),
            state,
            path: path.into(),
            themes: crate::theme::THEMES
                .iter()
                .map(|p| Chip {
                    name: p.name,
                    current: p.name == current,
                })
                .collect(),
        }
    }

    /// The line in the card's header. `email · tier`, the way the page has
    /// always said it.
    fn signed(mut self, email: Option<&str>, tier: Option<Tier>) -> Shell {
        self.who = match (email, tier) {
            (Some(email), Some(tier)) => format!("{email} · {}", tier.name()),
            (Some(email), None) => email.to_string(),
            _ => String::new(),
        };
        self
    }
}

// -------------------------------------------------------------- the errors ---

/// A refusal with a way out of it.
///
/// The API answers a failure with a status and a code, because something is
/// reading it. A person is reading this, so it is the error pane with the
/// sentence in it — the same sentence, kept whole from `ga.rs` — and a link
/// back to wherever trying again makes sense.
struct Oops {
    shell: Shell,
    message: String,
    back: String,
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorView {
    shell: Shell,
    message: String,
    back: String,
}

impl IntoResponse for Oops {
    fn into_response(self) -> Response {
        let status = if self.message.contains("needs the ") {
            StatusCode::PAYMENT_REQUIRED
        } else {
            StatusCode::BAD_GATEWAY
        };
        let view = ErrorView {
            shell: self.shell,
            message: self.message,
            back: self.back,
        };
        (status, page(view)).into_response()
    }
}

type View = std::result::Result<Response, Oops>;

/// Renders, or says plainly that it could not.
///
/// A template that fails to render is a bug in this crate, not a thing that
/// happens to a user — every one of them is checked at compile time — so this
/// does not dress it up.
fn page<T: Template>(view: T) -> Response {
    match view.render() {
        Ok(html) => Html(html).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("this page failed to render: {err}"),
        )
            .into_response(),
    }
}

/// The same [`super::Fail`] conversion, in the voice a page uses: the whole
/// message, because `ga.rs` already wrote it for somebody to read.
fn why(err: anyhow::Error) -> String {
    err.chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(": ")
}

// --------------------------------------------------------------- the guard ---

/// The cookie, the origin, and the idle clock, in front of every view but the
/// way in.
///
/// A missing cookie is not an error here — it is somebody who opened
/// `127.0.0.1:52413/properties` from their history, a day and a port later.
/// They go to `/`, which knows how to ask for the token.
async fn keyed(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    app.last.store(now(), Ordering::Relaxed);

    if let Some(origin) = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        if !allowed(&app, origin) {
            return (
                StatusCode::FORBIDDEN,
                format!("{origin} is not an origin this server answers."),
            )
                .into_response();
        }
    }

    if !holds(&app, &request) {
        return Redirect::to("/").into_response();
    }
    next.run(request).await
}

/// This run's cookie name: the one above, with this server's port on it.
fn key_of(origin: &str) -> String {
    format!("{KEY}_{}", origin.rsplit(':').next().unwrap_or("0"))
}

/// Whether the request carries this server's token in its cookie jar.
fn holds(app: &App, request: &Request) -> bool {
    let key = key_of(&app.origin);
    request
        .headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|jar| jar.split(';'))
        .filter_map(|crumb| crumb.trim().split_once('='))
        .any(|(name, value)| name == key && super::same(value, &app.token))
}

/// `HttpOnly` so no script can read the token back out, `SameSite=Strict` so
/// no other origin's form can spend it, and no `Secure` because this server
/// is http on loopback and a `Secure` cookie would simply never be stored.
fn wear(origin: &str, token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{}={token}; Path=/; HttpOnly; SameSite=Strict",
        key_of(origin)
    ))
    .expect("a minted token is ASCII")
}

// ------------------------------------------------------------- the way in ---

#[derive(Template)]
#[template(path = "start.html")]
struct StartView {
    shell: Shell,
    heading: String,
}

async fn stylesheet() -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], STYLE).into_response()
}

/// Where the terminal's URL lands, and the only route that decides where
/// somebody should be rather than showing them something.
///
/// It is also where every "check again" goes, because the answer to *has
/// anything changed* is always *let this route look*.
async fn landing(State(app): State<Arc<App>>, request: Request) -> View {
    if !holds(&app, &request) {
        // `?bad=1` is the handshake saying it tried: the token in the
        // fragment was not this server's, which is worth a different sentence
        // from never having had one.
        let bad = request.uri().query().is_some_and(|q| q.contains("bad=1"));
        return Ok(page(StartView {
            shell: Shell::new(if bad { "err" } else { "boot" }, "/"),
            heading: if bad {
                "That is not this server's token".into()
            } else {
                "Open the URL the terminal printed".into()
            },
        }));
    }

    let stand = stand(&app).await.map_err(|message| Oops {
        shell: Shell::new("err", "/"),
        message,
        back: "/".into(),
    })?;

    Ok(Redirect::to(where_to(&stand)).into_response())
}

/// The view somebody in this position belongs on. One sentence, in one place,
/// because two routes ask it: `/`, which redirects, and the handshake, which
/// cannot.
fn where_to(stand: &Stand) -> &'static str {
    match stand {
        Stand::Pending => "/signin/waiting",
        Stand::Out { .. } => "/signin",
        Stand::Unpaid { .. } => "/unlock",
        Stand::In { .. } => "/properties",
    }
}

#[derive(Deserialize)]
struct Key {
    k: String,
}

/// The fragment, handed over once. Answers with the cookie, and with where to
/// go next.
///
/// Not a redirect, and that is the whole of why this route exists in this
/// shape. A browser told to follow a redirect carries the fragment of the
/// page it started on onto the destination — so a handshake that ended in a
/// `303` would put the token straight back in the address bar it was just
/// taken out of. Naming the destination instead lets the script land on it in
/// one navigation, which drops the fragment for good.
async fn hand_over(State(app): State<Arc<App>>, request: Request) -> Response {
    // The same check the guard makes, made here because this route is in
    // front of the guard: it is how somebody gets past it.
    if let Some(origin) = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        if !allowed(&app, origin) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    let Ok(Form(body)) = Form::<Key>::from_request(request, &()).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !super::same(&body.k, &app.token) {
        return StatusCode::FORBIDDEN.into_response();
    }
    // A stand this cannot read is not a refusal: `/` will render the same
    // failure with the words around it.
    let next = stand(&app).await.map_or("/", |stand| where_to(&stand));
    let mut response =
        ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], next).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, wear(&app.origin, &app.token));
    response
}

// -------------------------------------------------------------- where we are ---

/// Where somebody stands with the CLI, which is the only question `/` asks.
enum Stand {
    /// A sign-in is running in another window.
    Pending,
    Out {
        failure: Option<String>,
    },
    Unpaid {
        email: Option<String>,
        tier: Option<Tier>,
    },
    In {
        email: Option<String>,
        tier: Option<Tier>,
    },
}

async fn stand(app: &App) -> std::result::Result<Stand, String> {
    if app.demo {
        return Ok(Stand::In {
            email: None,
            tier: Some(Tier::Elite),
        });
    }
    let signed_in = Tokens::load().map_err(why)?.is_some();
    if !signed_in {
        let login = app.login.lock().expect("the login lock is never poisoned");
        return Ok(match &*login {
            Login::Pending => Stand::Pending,
            Login::Failed(err) => Stand::Out {
                failure: Some(err.clone()),
            },
            Login::Idle => Stand::Out { failure: None },
        });
    }

    let account = Auth::account().map_err(why)?;
    let email = account.and_then(|a| a.email);
    // Asked rather than cached: a page about to offer a checkout should not
    // offer one to somebody who paid on another laptop ten seconds ago.
    let tier = license::sync(&Config::load().map_err(why)?).await;
    Ok(if tier.is_some_and(|have| have.meets(PLAN)) {
        Stand::In { email, tier }
    } else {
        Stand::Unpaid { email, tier }
    })
}

// ------------------------------------------------------------ signing in ---

#[derive(Template)]
#[template(path = "signin.html")]
struct SignInView {
    shell: Shell,
    note: String,
}

#[derive(Template)]
#[template(path = "waiting.html")]
struct WaitingView {
    shell: Shell,
    /// Seconds between the browser asking again.
    every: u32,
    what: String,
    note: String,
    giveup: &'static str,
    giveup_label: &'static str,
}

async fn signin(State(app): State<Arc<App>>) -> View {
    let failure = match stand(&app).await {
        Ok(Stand::Out { failure }) => failure,
        // Signed in already, or mid-flight: `/` is the route that knows.
        Ok(_) => return Ok(Redirect::to("/").into_response()),
        Err(message) => {
            return Err(Oops {
                shell: Shell::new("err", "/signin"),
                message,
                back: "/".into(),
            })
        }
    };
    Ok(page(SignInView {
        shell: Shell::new("out", "/signin"),
        note: match failure {
            Some(err) => format!("last attempt: {err}"),
            None => "craft serve is holding the credentials; this page never sees them.".into(),
        },
    }))
}

/// Start the Google sign-in and get out of the way.
///
/// The flow blocks on a loopback accept — it is waiting for a person to click
/// things — so it runs on a thread with a runtime of its own, exactly as
/// [`super::session_start`] does, and this redirects to the page that waits.
async fn signin_start(State(app): State<Arc<App>>) -> Response {
    if app.demo || Tokens::load().ok().flatten().is_some() {
        return Redirect::to("/").into_response();
    }
    {
        let mut login = app.login.lock().expect("the login lock is never poisoned");
        if !matches!(*login, Login::Pending) {
            *login = Login::Pending;
            let state = app.clone();
            // Where Google's tab is sent afterwards. A view has a URL, so this
            // is now an address rather than the page plus a fragment: the
            // cookie is already in the browser that started this.
            let back = format!("{}/", state.origin);
            std::thread::spawn(move || {
                let outcome = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(anyhow::Error::from)
                    .and_then(|rt| {
                        rt.block_on(async {
                            let auth = Auth::new(reqwest::Client::new())?;
                            auth.login_landing(&Landing {
                                title: "Signed in",
                                body: "anacraft has your Google account. \
                                       Taking you to your properties…",
                                redirect: Some(&back),
                                cta: Some(Cta {
                                    label: "Your properties →",
                                    url: &back,
                                    note: "if this page has not moved along by itself",
                                }),
                            })
                            .await?;
                            // The same courtesy `craft login` does: register
                            // the account so a subscription bought anywhere
                            // finds it here.
                            if let Some(account) = Auth::account()? {
                                let _ = license::link(&account).await;
                                let _ = license::sync(&Config::load()?).await;
                            }
                            Ok::<(), anyhow::Error>(())
                        })
                    });
                *state
                    .login
                    .lock()
                    .expect("the login lock is never poisoned") = match outcome {
                    Ok(()) => Login::Idle,
                    Err(err) => Login::Failed(why(err)),
                };
            });
        }
    }
    Redirect::to("/signin/waiting").into_response()
}

async fn signin_waiting(State(app): State<Arc<App>>) -> View {
    // Still waiting is the only reason to render this; anything else is `/`'s
    // business, and the meta refresh will carry them there on its own.
    if !matches!(stand(&app).await, Ok(Stand::Pending)) {
        return Ok(Redirect::to("/").into_response());
    }
    Ok(page(WaitingView {
        shell: Shell::new("boot", "/signin/waiting"),
        every: 1,
        what: "waiting for Google — finish in the window that opened".into(),
        note: "This page asks the CLI once a second and moves on by itself.".into(),
        giveup: "/signin",
        giveup_label: "start again",
    }))
}

/// Revoke the credentials and forget them, then start over at `/` — which
/// will send them to the sign-in, because that is now where they stand.
async fn signout(State(app): State<Arc<App>>) -> View {
    let oops = |err: anyhow::Error| Oops {
        shell: Shell::new("err", "/"),
        message: why(err),
        back: "/".into(),
    };
    if !app.demo {
        let auth = Auth::new(reqwest::Client::new()).map_err(oops)?;
        auth.logout().await.map_err(oops)?;
        let _ = license::forget();
    }
    Ok(Redirect::to("/").into_response())
}

// ---------------------------------------------------------------- paying ---

#[derive(Template)]
#[template(path = "unlock.html")]
struct UnlockView {
    shell: Shell,
    heading: &'static str,
    body: &'static str,
    price: String,
    action: &'static str,
    who: String,
}

#[derive(Template)]
#[template(path = "checkout.html")]
struct CheckoutView {
    shell: Shell,
    url: String,
}

async fn unlock(State(app): State<Arc<App>>) -> View {
    let shell = Shell::new("pay", "/unlock");
    let (email, tier) = match stand(&app).await {
        Ok(Stand::Unpaid { email, tier }) => (email, tier),
        // Paid up, or not signed in at all: `/` knows which, and a paywall is
        // the wrong thing to show either of them.
        Ok(_) => return Ok(Redirect::to("/").into_response()),
        Err(message) => {
            return Err(Oops {
                shell,
                message,
                back: "/".into(),
            })
        }
    };

    // `tier` rather than `entitled`: somebody already subscribed to a smaller
    // plan is being asked to move up, which is a different sentence and a
    // different button from being asked to subscribe.
    let subscribed = tier.is_some();
    Ok(page(UnlockView {
        shell: shell.signed(email.as_deref(), tier),
        heading: if subscribed {
            "This needs the Elite plan"
        } else {
            "Unlock the tag"
        },
        body: if subscribed {
            "Your subscription is live, but the API is on Anacrafter Elite — the same plan \
             craft mcp is on. Moving up is billed going forward, not charged again."
        } else {
            "The API is part of Anacrafter Elite — the same plan craft mcp is on, so one \
             subscription covers both the tag and the assistant that reads the numbers \
             afterwards."
        },
        price: PLAN.monthly().to_string(),
        action: if subscribed {
            "Move up to Elite →"
        } else {
            "Continue to checkout →"
        },
        who: match email {
            Some(email) => format!("signed in as {email}"),
            None => String::new(),
        },
    }))
}

/// A checkout URL already tied to the signed-in account, so the payment lands
/// on the row every `craft` command reads afterwards. The same two steps
/// `craft subscribe` takes, and the same two [`super::checkout`] takes.
async fn unlock_checkout(State(app): State<Arc<App>>) -> View {
    let shell = Shell::new("pay", "/unlock");
    let oops = |message: String| Oops {
        shell: Shell::new("err", "/unlock"),
        message,
        back: "/unlock".into(),
    };
    if app.demo {
        return Err(oops(
            "this server is running with --demo, which changes nothing on this machine \
             or in any Analytics account, so starting a checkout is not something it can do."
                .into(),
        ));
    }
    let account = Auth::account()
        .map_err(|err| oops(why(err)))?
        .ok_or_else(|| oops("nothing is signed in yet.".into()))?;

    let token = license::mint_token();
    // Best-effort, exactly as in the CLI: a claim that fails leaves the
    // checkout claimable by the email on it.
    let _ = license::claim(&token, &account).await;

    Ok(page(CheckoutView {
        shell,
        url: license::checkout_url(crate::subscribe_url(PLAN), &token, account.email.as_deref()),
    }))
}

// ------------------------------------------------------------ properties ---

pub(super) struct Row {
    id: String,
    name: String,
    account: String,
    /// Where the trash link goes, built here rather than in the template:
    /// the name rides in the query so the confirmation page can say which
    /// property it means, and a name with an `&` in it has to be
    /// percent-encoded, which escaping for HTML does not do.
    trash: String,
}

impl Row {
    fn new(id: String, name: String, account: String) -> Row {
        Row {
            trash: format!("/properties/{id}/trash?n={}", license::encode(&name)),
            id,
            name,
            account,
        }
    }
}

#[derive(Template)]
#[template(path = "properties.html")]
struct PropertiesView {
    shell: Shell,
    properties: Vec<Row>,
}

async fn properties(State(app): State<Arc<App>>) -> View {
    let shell = Shell::new("pick", "/properties");
    let oops = |message: String| Oops {
        shell: Shell::new("err", "/properties"),
        message,
        back: "/".into(),
    };

    // Asked here rather than assumed: somebody who signed out in another tab,
    // or whose plan lapsed while this one sat open, belongs at `/` and not on
    // a list this login can no longer read.
    let (email, tier) = match stand(&app).await {
        Ok(Stand::In { email, tier }) => (email, tier),
        Ok(_) => return Ok(Redirect::to("/").into_response()),
        Err(message) => return Err(oops(message)),
    };

    if app.demo {
        return Ok(page(PropertiesView {
            shell,
            properties: vec![Row::new(
                "demo".into(),
                "Contoso Labs (demo)".into(),
                "Anacraft demo".into(),
            )],
        }));
    }

    let ga = client().await.map_err(|fail| oops(fail.message))?;
    let found = ga.properties().await.map_err(|err| oops(why(err)))?;

    Ok(page(PropertiesView {
        shell: shell.signed(email.as_deref(), tier),
        properties: found
            .into_iter()
            .map(|p| Row::new(p.id, p.name, p.account))
            .collect(),
    }))
}

#[derive(Template)]
#[template(path = "new.html")]
struct NewView {
    shell: Shell,
    heading: String,
    body: String,
    action: String,
    submit: &'static str,
    url: String,
}

#[derive(Deserialize)]
struct Site {
    url: String,
}

async fn new_property() -> View {
    Ok(page(NewView {
        shell: Shell::new("pick", "/properties/new"),
        heading: "A new property".into(),
        body: "anacraft creates the property and its web data stream, then reads the \
               measurement ID back."
            .into(),
        action: "/properties".into(),
        submit: "Create the property",
        url: String::new(),
    }))
}

/// The whole point of this server, in one form.
///
/// [`crate::configure::setup`] does the deciding — reuse what already
/// measures this host, finish a property that never got a stream, or create
/// both — so this is the same behaviour `craft configure` has and the same
/// [`super::register`] answers in JSON.
async fn create(State(app): State<Arc<App>>, Form(body): Form<Site>) -> View {
    let oops = |message: String| Oops {
        shell: Shell::new("err", "/properties/new"),
        message,
        back: "/properties/new".into(),
    };
    let host = crate::configure::host_of(&body.url).map_err(|err| oops(err.to_string()))?;

    if app.demo {
        return Ok(Redirect::to(&format!(
            "/tag/G-DEMO1A2B3C4D?p=3900000000&n={}",
            license::encode(&host)
        ))
        .into_response());
    }
    require("registering a tag")
        .await
        .map_err(|fail| oops(fail.message))?;

    let ga = client().await.map_err(|fail| oops(fail.message))?;
    let setup = crate::configure::setup(
        &ga,
        &host,
        crate::configure::Options {
            account: None,
            timezone: None,
            currency: "USD".to_string(),
        },
        crate::configure::Consent::HeldOnly,
    )
    .await
    .map_err(|err| oops(why(err)))?;

    Ok(Redirect::to(&format!(
        "/tag/{}?p={}&n={}",
        setup.stream.measurement_id,
        setup.property.id,
        license::encode(&setup.property.name),
    ))
    .into_response())
}

// --------------------------------------------------------- the data streams ---

pub(super) struct Web {
    measurement_id: String,
    name: String,
}

#[derive(Template)]
#[template(path = "streams.html")]
struct StreamsView {
    shell: Shell,
    property_id: String,
    streams: Vec<Web>,
}

/// One stream is not a choice, and none is a form. Only the middle case is a
/// page, which is why this route redirects more often than it renders.
async fn streams(State(app): State<Arc<App>>, Path(id): Path<String>) -> View {
    let property = bare(&id);
    let oops = |message: String| Oops {
        shell: Shell::new("err", "/properties"),
        message,
        back: "/properties".into(),
    };

    if app.demo {
        return Ok(
            Redirect::to("/tag/G-DEMO1A2B3C4D?p=demo&n=Contoso%20Labs%20%28demo%29")
                .into_response(),
        );
    }

    let ga = client().await.map_err(|fail| oops(fail.message))?;
    let found = ga
        .web_streams(&property)
        .await
        .map_err(|err| oops(why(err)))?;
    let cfg = Config::load().unwrap_or_default();
    let named = cfg.find(&property).map(|p| p.display()).unwrap_or_default();

    if found.len() == 1 {
        return Ok(Redirect::to(&format!(
            "/tag/{}?p={property}&n={}",
            found[0].measurement_id,
            license::encode(&named),
        ))
        .into_response());
    }
    if found.is_empty() {
        return Ok(page(NewView {
            shell: Shell::new("pick", format!("/properties/{property}/streams")),
            heading: "This property measures no website yet".into(),
            body: format!(
                "“{named}” has no web data stream, so there is no measurement ID on it. \
                 Adding one mints it."
            ),
            action: format!("/properties/{property}/streams"),
            submit: "Add the web stream",
            url: String::new(),
        }));
    }

    Ok(page(StreamsView {
        shell: Shell::new("pick", format!("/properties/{property}/streams")),
        property_id: property,
        streams: found
            .into_iter()
            .map(|s| Web {
                // The URL says more than `name` does when there are several
                // of them, which is the only case this view is rendered in.
                name: if s.default_uri.is_empty() {
                    s.name
                } else {
                    s.default_uri
                },
                measurement_id: s.measurement_id,
            })
            .collect(),
    }))
}

async fn add_stream(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Form(body): Form<Site>,
) -> View {
    let property = bare(&id);
    let back = format!("/properties/{property}/streams");
    let oops = |message: String| Oops {
        shell: Shell::new("err", &back),
        message,
        back: back.clone(),
    };
    let host = crate::configure::host_of(&body.url).map_err(|err| oops(err.to_string()))?;

    if app.demo {
        return Ok(Redirect::to(&format!(
            "/tag/G-DEMO1A2B3C4D?p={property}&n={}",
            license::encode(&host)
        ))
        .into_response());
    }
    require("adding a web stream")
        .await
        .map_err(|fail| oops(fail.message))?;

    let ga = client().await.map_err(|fail| oops(fail.message))?;
    let stream = ga
        .create_web_stream(&property, &host, &format!("https://{host}"))
        .await
        .map_err(|err| oops(why(err)))?;

    Ok(Redirect::to(&format!(
        "/tag/{}?p={property}&n={}",
        stream.measurement_id,
        license::encode(&host),
    ))
    .into_response())
}

// --------------------------------------------------------------- throwing away ---

#[derive(Template)]
#[template(path = "trash.html")]
struct TrashView {
    shell: Shell,
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct Named {
    #[serde(default)]
    n: Option<String>,
}

async fn trash_ask(Path(id): Path<String>, Query(named): Query<Named>) -> View {
    let property = bare(&id);
    let name = named
        .n
        .or_else(|| {
            Config::load()
                .ok()
                .and_then(|cfg| cfg.find(&property).map(|p| p.display()))
        })
        .unwrap_or_else(|| format!("property {property}"));
    Ok(page(TrashView {
        shell: Shell::new(
            "pick",
            format!("/properties/{property}/trash?n={}", license::encode(&name)),
        ),
        id: property,
        name,
    }))
}

#[derive(Deserialize)]
struct Confirm {
    confirm: String,
}

/// The one destructive route here, and the same one `craft delete --all`
/// makes. Google's delete is a soft one: the property sits in the account's
/// trash for 35 days, restorable from the console, before anything is gone.
///
/// It is opt-in twice, the way the command is and the way [`super::trash`]
/// is: a page to say it on, and the id said again in the form.
async fn trash_do(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Form(body): Form<Confirm>,
) -> View {
    let property = bare(&id);
    let oops = |message: String| Oops {
        shell: Shell::new("err", "/properties"),
        message,
        back: "/properties".into(),
    };
    if bare(&body.confirm) != property {
        return Err(oops(
            "that form named a different property than the one it was on.".into(),
        ));
    }
    if app.demo {
        return Err(oops(
            "this server is running with --demo, which changes nothing in any Analytics \
             account, so deleting a property is not something it can do."
                .into(),
        ));
    }

    let ga = client().await.map_err(|fail| oops(fail.message))?;
    ga.delete_property(&property)
        .await
        .map_err(|err| oops(why(err)))?;

    // Google first, then here — the same order the command uses, because
    // forgetting is local and reversible and a failed API call is not a
    // reason to have already pointed the dashboard away from a property that
    // is still sitting there collecting.
    let mut cfg = Config::load().map_err(|err| oops(why(err)))?;
    if cfg.remove(&property) {
        cfg.save().map_err(|err| oops(why(err)))?;
    }
    Ok(Redirect::to("/properties").into_response())
}

// ------------------------------------------------------------------ the tag ---

#[derive(Template)]
#[template(path = "tag.html")]
struct TagView {
    shell: Shell,
    measurement_id: String,
    subtitle: String,
    /// This view's own address, minus the tab and the flag, so the links on
    /// it can be built by adding one.
    here: String,
    prompt_shown: bool,
    text: String,
    copy_label: &'static str,
    hint: &'static str,
    next_line: &'static str,
    can_default: bool,
    property_id: String,
}

#[derive(Deserialize)]
struct Which {
    /// The property the tag belongs to, and its name — carried in the URL so
    /// this view can be reloaded, bookmarked or sent to somebody without a
    /// second round trip to Google to ask what the property was called.
    #[serde(default)]
    p: Option<String>,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    show: Option<String>,
    /// Set by the redirect after saving the default, so the line under the
    /// command can say so.
    #[serde(default)]
    saved: Option<String>,
}

async fn tag(Path(measurement_id): Path<String>, Query(which): Query<Which>) -> View {
    let id = measurement_id.trim().to_uppercase();
    if !id.starts_with("G-") || id.len() < 4 {
        return Err(Oops {
            shell: Shell::new("err", "/properties"),
            message: "a measurement id looks like G-XXXXXXXXXX".into(),
            back: "/properties".into(),
        });
    }

    let property = which.p.map(|p| bare(&p)).unwrap_or_default();
    let name = which.n.unwrap_or_default();
    let prompt_shown = which.show.as_deref() != Some("tag");

    // Both blocks are the binary's, so the terminal, the MCP tool and this
    // page cannot drift into handing out three slightly different tags.
    let snippet = crate::configure::tag_snippet(&id);
    let text = if prompt_shown {
        crate::configure::tag_prompt(&id)
    } else {
        snippet
    };

    let here = format!(
        "/tag/{id}?p={}&n={}",
        license::encode(&property),
        license::encode(&name)
    );
    // Where the browser is actually standing, tab and flag and all, so a
    // palette chip puts it back on this page rather than on the list.
    let mut standing = here.clone();
    if !prompt_shown {
        standing.push_str("&show=tag");
    }
    if which.saved.is_some() {
        standing.push_str("&saved=1");
    }

    Ok(page(TagView {
        shell: Shell::new("out", standing),
        subtitle: match (name.is_empty(), property.is_empty()) {
            (false, false) => format!("{name} · property {property}"),
            (true, false) => format!("property {property}"),
            _ => String::new(),
        },
        measurement_id: id,
        here,
        prompt_shown,
        text,
        copy_label: if prompt_shown {
            "Copy for the agent"
        } else {
            "Copy the tag"
        },
        hint: if prompt_shown {
            "Paste it into the chat of whatever built the site — Lovable, v0, Bolt — and send."
        } else {
            "Into <head>, on every page, and only once: a second GA4 tag doubles every number."
        },
        next_line: if which.saved.is_some() {
            "Saved as the default. From any terminal:"
        } else {
            "Read it from the terminal whenever you like:"
        },
        can_default: !property.is_empty() && which.saved.is_none(),
        property_id: property,
    }))
}

// ------------------------------------------------------- the two small writes ---

/// Where a form sends the browser afterwards.
///
/// A path on this server and nothing else. The value is this server's own —
/// every template writes it from a route it already knows — but it arrives
/// back over the wire, and a redirect that will follow anything it is handed
/// is an open redirect whether or not anybody meant it to be.
fn back_to(given: Option<&str>) -> String {
    given
        .filter(|back| back.starts_with('/') && !back.starts_with("//"))
        .unwrap_or("/")
        .to_string()
}

#[derive(Deserialize)]
struct Chosen {
    id: String,
    #[serde(default)]
    back: Option<String>,
}

/// `craft use`, over a form. It checks the property is one this login can see
/// before writing it down, for the same reason the command does: a default
/// nothing can read is a dashboard that opens on an error.
async fn use_property(State(app): State<Arc<App>>, Form(body): Form<Chosen>) -> View {
    let back = back_to(body.back.as_deref());
    let oops = |message: String| Oops {
        shell: Shell::new("err", "/properties"),
        message,
        back: "/properties".into(),
    };
    if app.demo {
        return Ok(Redirect::to(&back).into_response());
    }

    let wanted = bare(&body.id);
    let ga = client().await.map_err(|fail| oops(fail.message))?;
    let found = ga
        .properties()
        .await
        .map_err(|err| oops(why(err)))?
        .into_iter()
        .find(|p| p.id == wanted)
        .ok_or_else(|| oops(format!("no property {wanted} on this account")))?;

    let mut cfg = Config::load().map_err(|err| oops(why(err)))?;
    cfg.upsert(&found.id, Some(found.name));
    cfg.save().map_err(|err| oops(why(err)))?;
    Ok(Redirect::to(&back).into_response())
}

#[derive(Deserialize)]
struct Palette {
    name: String,
    #[serde(default)]
    back: Option<String>,
}

/// The page wears whichever palette the CLI is set to rather than keeping a
/// preference of its own: the port changes every run, so anything this page
/// stored would be stored against an origin that will not exist tomorrow.
/// The config file is the one place a choice survives, and it is the same
/// line `craft theme` writes and the dashboard reads.
async fn use_theme(State(app): State<Arc<App>>, Form(body): Form<Palette>) -> View {
    let back = back_to(body.back.as_deref());
    if !crate::theme::select(&body.name) {
        return Err(Oops {
            shell: Shell::new("err", &back),
            message: format!("no palette called {}", body.name),
            back,
        });
    }
    // A demo changes nothing on this machine, and a line in the config file
    // is something on this machine. The run still wears it; it just does not
    // outlive the run.
    if !app.demo {
        if let Ok(mut cfg) = Config::load() {
            cfg.theme = Some(body.name);
            let _ = cfg.save();
        }
    }
    Ok(Redirect::to(&back).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shell with nothing signed in, which is all these tests need: they
    /// are about the markup, not about who is looking at it.
    fn shell() -> Shell {
        Shell::new("pick", "/properties")
    }

    /// This module's own source, read at compile time, so the templates can
    /// be checked against the routes they point at. The same trick the parent
    /// module uses on its OpenAPI table, for the same reason: nothing else
    /// notices when a template and a router part company.
    const SOURCE: &str = include_str!("views.rs");

    /// Every path handed to `route(`, above the test module.
    fn routed() -> Vec<String> {
        let code = SOURCE
            .split_once("\n#[cfg(test)]")
            .map(|(code, _)| code)
            .expect("this module is the first test module in the file")
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        code.match_indices(".route(")
            .map(|(at, _)| {
                let rest = &code[at + ".route(".len()..];
                let open = rest.find('"').expect("a route path is a string literal") + 1;
                let rest = &rest[open..];
                rest[..rest.find('"').expect("an unterminated route path")].to_string()
            })
            .map(|path| shape(&path))
            .collect()
    }

    /// A path with the varying parts taken out, so `/properties/:id/streams`
    /// and `/properties/3971/streams` compare equal.
    fn shape(path: &str) -> String {
        path.split('?')
            .next()
            .unwrap_or("")
            .split('/')
            .map(|segment| {
                if segment.starts_with(':')
                    || segment.contains("{{")
                    || segment.chars().any(|c| c.is_ascii_digit())
                    || segment.starts_with("G-")
                {
                    ":x"
                } else {
                    segment
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    /// The templates, by the name askama knows them by.
    const TEMPLATES: [(&str, &str); 12] = [
        ("layout.html", include_str!("../../templates/layout.html")),
        ("start.html", include_str!("../../templates/start.html")),
        ("signin.html", include_str!("../../templates/signin.html")),
        ("waiting.html", include_str!("../../templates/waiting.html")),
        ("unlock.html", include_str!("../../templates/unlock.html")),
        (
            "checkout.html",
            include_str!("../../templates/checkout.html"),
        ),
        (
            "properties.html",
            include_str!("../../templates/properties.html"),
        ),
        ("streams.html", include_str!("../../templates/streams.html")),
        ("new.html", include_str!("../../templates/new.html")),
        ("trash.html", include_str!("../../templates/trash.html")),
        ("tag.html", include_str!("../../templates/tag.html")),
        ("error.html", include_str!("../../templates/error.html")),
    ];

    /// Every local address a template points at: form targets and links, but
    /// not the guide, which is somewhere else on purpose.
    fn pointed_at(template: &str) -> Vec<String> {
        let mut found = Vec::new();
        for attribute in ["action=\"", "href=\""] {
            let mut rest = template;
            while let Some(at) = rest.find(attribute) {
                rest = &rest[at + attribute.len()..];
                let value = &rest[..rest.find('"').expect("an unterminated attribute")];
                if value.starts_with('/') {
                    found.push(value.to_string());
                }
            }
        }
        found
    }

    #[test]
    fn every_address_the_templates_point_at_is_one_this_module_routes() {
        // The templates are files; nothing type-checks the paths in them.
        // This does — and it is the check the old single page could only make
        // by searching itself for substrings.
        let routes = routed();
        for (name, template) in TEMPLATES {
            for target in pointed_at(template) {
                // `/docs` is the parent module's, and `/app.css` is routed
                // here; both come out of the scan below as themselves.
                if target == "/docs" {
                    continue;
                }
                assert!(
                    routes.contains(&shape(&target)),
                    "{name} points at {target} and nothing here routes it"
                );
            }
        }
    }

    #[test]
    fn every_view_this_module_renders_is_reachable_from_another_one() {
        // A view nothing links to is a view nobody finds. The exceptions are
        // the three a redirect lands on rather than a link: the way in, the
        // list, and the tag itself.
        let entrances = [
            "/",
            "/properties",
            "/tag/:x",
            "/signin/waiting",
            "/session/key",
        ];
        let linked: Vec<String> = TEMPLATES
            .iter()
            .flat_map(|(_, template)| pointed_at(template))
            .map(|target| shape(&target))
            .collect();
        for route in routed() {
            assert!(
                linked.contains(&route) || entrances.contains(&route.as_str()),
                "{route} is routed and no template points at it"
            );
        }
    }

    #[test]
    fn the_views_fetch_nothing_from_anywhere_else() {
        // `craft serve` runs on machines behind corporate proxies, on
        // aeroplanes, and on a laptop whose only working connection is the
        // one to Google. A stylesheet or a font from a CDN would turn the
        // page that hands over a measurement id into a page that sometimes
        // renders.
        let rendered = ErrorView {
            shell: shell(),
            message: "something went wrong".into(),
            back: "/".into(),
        }
        .render()
        .expect("the error view renders");

        for tag in ["<script src", "@import", "fonts.googleapis", "cdn."] {
            assert!(
                !rendered.contains(tag),
                "a rendered view reaches for `{tag}` — these pages have to be self-contained"
            );
        }
        // The one thing the browser is asked to fetch is this server's own
        // stylesheet, and the one outbound address is a link a person clicks.
        assert_eq!(rendered.matches("<link").count(), 1);
        assert!(rendered.contains("href=\"/app.css\""));
        assert_eq!(rendered.matches("href=\"http").count(), 1);
        assert!(rendered.contains("href=\"https://anacraft.dev/serve.html\""));
        assert!(!STYLE.contains("@import"));
        assert!(!STYLE.contains("url(http"));
    }

    #[test]
    fn a_property_name_arrives_as_text_and_never_as_markup() {
        // Property names are somebody's typing, and the reason these views
        // are templates rather than `format!` is that askama escapes by
        // default and a format string does not.
        let rendered = PropertiesView {
            shell: shell(),
            properties: vec![Row::new(
                "397412345".into(),
                "<img src=x onerror=alert(1)>".into(),
                "Acme & Co".into(),
            )],
        }
        .render()
        .expect("the property list renders");

        assert!(!rendered.contains("<img src=x"));
        assert!(rendered.contains("&lt;img src=x"));
        assert!(rendered.contains("Acme &amp; Co"));
    }

    #[test]
    fn the_tag_is_one_block_shown_two_ways() {
        let prompt = crate::configure::tag_prompt("G-1A2BCD345E");
        let snippet = crate::configure::tag_snippet("G-1A2BCD345E");
        // The prompt is the tag plus instructions, never a retyping of it, so
        // the two tabs cannot show two different measurement ids.
        assert!(prompt.contains(&snippet));
    }

    #[test]
    fn a_redirect_only_ever_goes_somewhere_on_this_server() {
        assert_eq!(back_to(Some("/properties")), "/properties");
        assert_eq!(back_to(Some("/tag/G-1?p=1")), "/tag/G-1?p=1");
        // The shapes an open redirect wears: another site, a
        // protocol-relative address, and a scheme of its own.
        assert_eq!(back_to(Some("//evil.example")), "/");
        assert_eq!(back_to(Some("https://evil.example")), "/");
        assert_eq!(back_to(Some("javascript:alert(1)")), "/");
        assert_eq!(back_to(None), "/");
    }

    #[test]
    fn the_cookie_cannot_be_read_by_a_script_or_sent_by_another_site() {
        let cookie = wear("http://127.0.0.1:52413", "abc123");
        let cookie = cookie.to_str().expect("a cookie is ASCII");
        // The port is in the name: two runs on one machine share a cookie jar
        // and must not share a cookie.
        assert!(cookie.starts_with("craft_key_52413=abc123;"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        // No `Secure`: this server is http on loopback, and a `Secure` cookie
        // would simply never be stored.
        assert!(!cookie.contains("Secure"));
    }
}
