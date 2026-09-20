//! OAuth 2.0 installed-application flow against Google, hand-rolled so the
//! whole login experience (including the browser success page) stays on-theme
//! and we carry no extra dependency surface.
//!
//! Google treats the client secret of a *Desktop* OAuth client as
//! non-confidential, which is what makes it safe to bake into a shipped binary.
//! We still use PKCE, which is what actually protects the exchange.

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Duration, Utc};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};

/// Analytics read and edit, plus the two non-sensitive OpenID scopes.
///
/// The identity scopes are not there to read anything about the person: they
/// are how a subscription survives a new laptop. Stripe's webhook writes the
/// Google account id against the payment, and a fresh machine that signs into
/// the same account gets its subscription back without anybody copying a token
/// around. `openid` and `email` are non-sensitive, so unlike a wider Analytics
/// scope they add nothing to the consent review — see the test below.
///
/// `analytics.edit` is included so `craft configure` never has to ask for
/// permission separately. It covers creating a property, adding a web data
/// stream, and deleting a property — the three Admin API calls this binary
/// makes. Asking once at login rather than mid-configure avoids a second
/// consent screen when somebody is already following the setup guide.
const SCOPE: &str = "openid email https://www.googleapis.com/auth/analytics.readonly \
     https://www.googleapis.com/auth/analytics.edit";

/// The write scope, included in [`SCOPE`] so login covers it from the start.
///
/// Creating a GA4 property and its web data stream is the whole reason it
/// exists: those two Admin API calls are documented as requiring
/// `analytics.edit`, and Google publishes no narrower "create a property"
/// scope to drop to. Included in the base login scope so `craft configure`
/// never has to provoke a second consent screen.
pub const SCOPE_EDIT: &str = "https://www.googleapis.com/auth/analytics.edit";

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";

/// Baked in at build time by the release pipeline:
///   ANACRAFT_OAUTH_CLIENT_ID=... ANACRAFT_OAUTH_CLIENT_SECRET=... cargo build --release
const BUILTIN_ID: Option<&str> = option_env!("ANACRAFT_OAUTH_CLIENT_ID");
const BUILTIN_SECRET: Option<&str> = option_env!("ANACRAFT_OAUTH_CLIENT_SECRET");

#[derive(Clone, Serialize, Deserialize)]
pub struct ClientCreds {
    pub client_id: String,
    pub client_secret: String,
}

impl ClientCreds {
    /// Env vars win (so contributors can point at their own project), then a
    /// local `client.json`, then whatever was compiled in.
    pub fn load() -> Result<ClientCreds> {
        if let (Ok(id), Ok(secret)) = (
            std::env::var("ANACRAFT_OAUTH_CLIENT_ID"),
            std::env::var("ANACRAFT_OAUTH_CLIENT_SECRET"),
        ) {
            if !id.trim().is_empty() {
                return Ok(ClientCreds {
                    client_id: id,
                    client_secret: secret,
                });
            }
        }

        let path = crate::config::home()?.join("client.json");
        if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            return serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", path.display()));
        }

        match (BUILTIN_ID, BUILTIN_SECRET) {
            (Some(id), Some(secret)) if !id.is_empty() => Ok(ClientCreds {
                client_id: id.to_string(),
                client_secret: secret.to_string(),
            }),
            _ => bail!(
                "no OAuth client configured.\n\n\
                 This build has no client baked in. Create a *Desktop app* OAuth client at\n\
                 https://console.cloud.google.com/apis/credentials, enable the Google Analytics\n\
                 Data API + Admin API, then either:\n\n  \
                 export ANACRAFT_OAUTH_CLIENT_ID=... ANACRAFT_OAUTH_CLIENT_SECRET=...\n\n\
                 or write ~/.anacraft/client.json:\n  \
                 {{\"client_id\": \"...\", \"client_secret\": \"...\"}}"
            ),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    /// Absent on credentials written before identity was asked for. Those still
    /// work for every report; only carrying a subscription to another machine
    /// needs a `craft login` to fill this in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<Account>,
    /// Space-separated scopes the consent screen actually granted, as Google
    /// reported them. Stored so `ensure_scope` can tell whether it has to ask
    /// for anything before a write, instead of provoking a 403 and explaining
    /// it afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl Tokens {
    fn path() -> Result<std::path::PathBuf> {
        Ok(crate::config::home()?.join("token.json"))
    }

    pub fn load() -> Result<Option<Tokens>> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&raw).ok())
    }

    pub fn save(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(self)?;
        crate::config::write_private(&Self::path()?, &raw)
    }

    pub fn clear() -> Result<()> {
        let path = Self::path()?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// Whether consent covered `scope`.
    ///
    /// Credentials written before scopes were recorded answer `false`. That is
    /// the safe direction: it costs one consent screen somebody can approve,
    /// where a wrong `true` would cost a 403 in the middle of the work.
    pub fn granted(&self, scope: &str) -> bool {
        self.scope
            .as_deref()
            .is_some_and(|granted| granted.split(' ').any(|s| s == scope))
    }

    /// Refresh a minute early so a long report can't expire mid-flight.
    fn is_stale(&self) -> bool {
        Utc::now() + Duration::seconds(60) >= self.expires_at
    }
}

/// Raw shape of Google's token endpoint response.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
    /// Present whenever `openid` was granted. Carries the account id, so no
    /// separate userinfo round trip is needed.
    #[serde(default)]
    id_token: Option<String>,
    /// What was granted, which is not always what was asked for: a person can
    /// untick a scope on the consent screen.
    #[serde(default)]
    scope: Option<String>,
}

/// Who Google says is signed in.
///
/// `sub` is Google's stable, opaque id for the account — it survives an email
/// change, which is exactly what a subscription needs to be keyed on. The email
/// rides along only so a support question ("which account did I pay with?") has
/// an answer a human recognises.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub sub: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// Read the account out of an id token.
///
/// The token came straight from Google's token endpoint over TLS, so the
/// signature is not re-checked here: there is no untrusted hop to protect
/// against, and a JWT library for one field would be a dependency for nothing.
/// A malformed token is simply no identity, never an error that blocks a login.
fn account_from_id_token(id_token: &str) -> Option<Account> {
    let payload = id_token.split('.').nth(1)?;
    let raw = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Account>(&raw)
        .ok()
        .filter(|a| !a.sub.is_empty())
}

pub struct Auth {
    http: reqwest::Client,
    creds: ClientCreds,
}

/// Which kind of trip through the consent screen this is.
///
/// `Additional` carries the sentence explaining what the extra permission is
/// for — a person asked for more access mid-command deserves to read why before
/// the browser opens rather than after — and the scope itself, so the check
/// that it was actually granted names it rather than inferring it from the
/// order of the request.
#[derive(Clone, Copy)]
enum Grant<'a> {
    Fresh,
    Additional { scope: &'a str, why: &'a str },
}

impl Auth {
    pub fn new(http: reqwest::Client) -> Result<Auth> {
        Ok(Auth {
            http,
            creds: ClientCreds::load()?,
        })
    }

    /// A valid bearer token, refreshing transparently when needed.
    pub async fn access_token(&self) -> Result<String> {
        let mut tokens = Tokens::load()?.context("not logged in — run `craft login`")?;

        if tokens.is_stale() {
            tokens = self.refresh(&tokens.refresh_token).await?;
            tokens.save()?;
        }
        Ok(tokens.access_token)
    }

    /// The signed-in Google account, if the stored credentials carry one.
    pub fn account() -> Result<Option<Account>> {
        Ok(Tokens::load()?.and_then(|t| t.account))
    }

    async fn refresh(&self, refresh_token: &str) -> Result<Tokens> {
        let res = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("client_id", self.creds.client_id.as_str()),
                ("client_secret", self.creds.client_secret.as_str()),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .context("contacting Google token endpoint")?;

        if !res.status().is_success() {
            let body = res.text().await.unwrap_or_default();
            // A revoked or expired refresh token is unrecoverable; make the
            // fix obvious instead of surfacing raw JSON.
            bail!("session expired — run `craft login` again\n  ({body})");
        }

        let body: TokenResponse = res.json().await?;
        Ok(Tokens {
            access_token: body.access_token,
            // Refresh responses omit refresh_token; keep the one we have.
            refresh_token: body
                .refresh_token
                .unwrap_or_else(|| refresh_token.to_string()),
            expires_at: Utc::now() + Duration::seconds(body.expires_in),
            // Google re-issues the id token on refresh only when `openid` was
            // granted at consent, so credentials from before the identity
            // scopes stay identity-less until the next `craft login`. That
            // costs them nothing but the cross-machine lookup — which is why
            // the existing account, if any, is kept rather than cleared.
            account: body
                .id_token
                .as_deref()
                .and_then(account_from_id_token)
                .or_else(|| Tokens::load().ok().flatten().and_then(|t| t.account)),
            // A refresh response repeats the granted scopes, but not on every
            // path; keeping the stored set when it doesn't is what stops a
            // refresh from silently "losing" a permission the user granted.
            scope: body
                .scope
                .or_else(|| Tokens::load().ok().flatten().and_then(|t| t.scope)),
        })
    }

    /// Full interactive login: PKCE + loopback redirect + browser handoff.
    pub async fn login(&self) -> Result<()> {
        self.login_landing(&Landing::plain(
            "Logged in",
            "anacraft is connected to your Google Analytics account. \
             You can close this tab and return to the terminal.",
        ))
        .await
    }

    /// The same login, leaving the browser somewhere the caller chooses.
    ///
    /// "Return to the terminal" is the right thing to say to somebody who
    /// typed `craft login` there, and the wrong thing to say to somebody who
    /// clicked a button on a page `craft serve` is holding open — they are
    /// not going back to a terminal, they are going back to the page, and it
    /// is one link away.
    pub async fn login_landing(&self, landing: &Landing<'_>) -> Result<()> {
        self.consent(SCOPE, Grant::Fresh).await?.show(landing);
        Ok(())
    }

    /// Ask for one scope more than the stored credentials carry, at the moment
    /// something actually needs it.
    ///
    /// This is Google's incremental authorization. Since `SCOPE` now carries the
    /// write scope, a fresh `craft login` is all `craft configure` needs — but
    /// credentials granted before the edit scope was part of login still land
    /// short of it, and `craft delete --all` and the tail of `craft configure`
    /// can hit that on a machine that signed in long ago. So this exists to top
    /// up what those old credentials hold, with `why` naming what it is about
    /// to do.
    ///
    /// The request re-sends the scopes already held plus the new one, and
    /// `include_granted_scopes=true` means the token that comes back covers
    /// both rather than replacing the old grant.
    ///
    /// What the browser is left looking at is the caller's to choose, and is
    /// chosen *after* this returns rather than before it is called: `craft
    /// configure` folds its subscription ask into this page, and whether it has
    /// anything to ask depends on who turned out to be signing in. So the tab
    /// is handed back still waiting, and [`Consented::show`] is what answers
    /// it — with [`GRANTED`] for every caller that knew all along.
    pub async fn ensure_scope(&self, scope: &str, why: &str) -> Result<Consented> {
        let stored = Tokens::load()?;
        if stored.as_ref().is_some_and(|t| t.granted(scope)) {
            return Ok(Consented::AlreadyHeld);
        }
        self.consent(
            &extend_scopes(stored.as_ref(), scope),
            Grant::Additional { scope, why },
        )
        .await
        .map(Consented::Granted)
    }

    /// One trip through the browser, for either kind of grant.
    ///
    /// Comes back with the tab still open. Everything the caller might want to
    /// decide the page on — who signed in, and so whether they are already a
    /// subscriber — is only knowable once the code below has been exchanged,
    /// which is after the browser is already sitting on the redirect. Telling
    /// it something before then is how a subscriber came to be shown a button
    /// asking them to subscribe.
    async fn consent(&self, scope: &str, grant: Grant<'_>) -> Result<Tab> {
        let Pkce {
            verifier,
            challenge,
        } = pkce();
        let state = nonce(24);

        // Port 0 lets the OS pick; Desktop clients accept any loopback port.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .context("could not open a local port for the OAuth redirect")?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}");

        let mut auth_url = format!(
            "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}\
             &code_challenge={}&code_challenge_method=S256&state={}\
             &access_type=offline&prompt=consent",
            encode(&self.creds.client_id),
            encode(&redirect_uri),
            encode(scope),
            encode(&challenge),
            encode(&state),
        );
        if let Grant::Additional { why, .. } = grant {
            auth_url.push_str("&include_granted_scopes=true");
            println!("  {} {why}", crate::theme::glyph::PICKAXE);
        }

        println!(
            "  {} opening your browser to {}…",
            crate::theme::glyph::PICKAXE,
            match grant {
                Grant::Fresh => "sign in with Google",
                Grant::Additional { .. } => "approve it with Google",
            }
        );
        println!("  if it doesn't open, paste this:\n\n  {auth_url}\n");
        let _ = open::that(&auth_url);

        let (code, tab) = wait_for_code(&listener, &state)?;

        let res = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("client_id", self.creds.client_id.as_str()),
                ("client_secret", self.creds.client_secret.as_str()),
                ("code", code.as_str()),
                ("code_verifier", verifier.as_str()),
                ("grant_type", "authorization_code"),
                ("redirect_uri", redirect_uri.as_str()),
            ])
            .send()
            .await?;

        if !res.status().is_success() {
            let body = res.text().await.unwrap_or_default();
            bail!("Google rejected the login: {body}");
        }

        let body: TokenResponse = res.json().await?;
        let granted = body.scope.clone().unwrap_or_else(|| scope.to_string());

        // `prompt=consent` means Google issues a refresh token every time,
        // including on the incremental grant. Falling back to the stored one
        // is belt and braces: re-consenting must never leave the install
        // unable to refresh.
        let stored = Tokens::load().ok().flatten();
        let refresh_token = body
            .refresh_token
            .or_else(|| stored.as_ref().map(|t| t.refresh_token.clone()))
            .ok_or_else(|| {
                anyhow!(
                    "Google did not return a refresh token — revoke anacraft's access at \
                     https://myaccount.google.com/permissions and try again"
                )
            })?;

        // A person can untick a scope on the consent screen, and the write
        // path has to hear about that here rather than as a 403 mid-run.
        if let Grant::Additional { scope: wanted, .. } = grant {
            if !granted.split(' ').any(|s| s == wanted) {
                bail!(
                    "that permission was not granted, so there is nothing to create with.\n  \
                     Nothing was changed. Re-run the command to see the screen again."
                );
            }
        }

        Tokens {
            access_token: body.access_token,
            refresh_token,
            expires_at: Utc::now() + Duration::seconds(body.expires_in),
            account: body
                .id_token
                .as_deref()
                .and_then(account_from_id_token)
                .or_else(|| stored.and_then(|t| t.account)),
            scope: Some(granted),
        }
        .save()?;

        Ok(tab)
    }

    /// Best-effort revoke, then drop local tokens regardless.
    pub async fn logout(&self) -> Result<()> {
        if let Some(tokens) = Tokens::load()? {
            let _ = self
                .http
                .post(REVOKE_URL)
                .form(&[("token", tokens.refresh_token.as_str())])
                .send()
                .await;
        }
        Tokens::clear()
    }
}

/// The scope set an incremental grant re-requests: everything the stored
/// credentials already carry, plus the one being added.
///
/// Google's incremental authorization wants the previously granted scopes
/// named again alongside the new one — `include_granted_scopes=true` then
/// keeps the exchange a superset rather than a replacement. Credentials that
/// record nothing (written before scopes were stored, or with an empty set)
/// fall back to the whole login set, which is the safe direction: it covers
/// whatever they lost, at the cost of a consent screen.
fn extend_scopes(stored: Option<&Tokens>, wanted: &str) -> String {
    let base = match stored
        .and_then(|t| t.scope.as_deref())
        .filter(|s| !s.trim().is_empty())
    {
        // Credentials that record their scopes: re-request exactly what was
        // granted plus the new one. Incremental authorization then keeps the
        // exchange a superset rather than a replacement.
        Some(granted) => granted.to_string(),
        // Nothing recorded: fall back to the whole login set — the safe
        // direction, since it covers whatever a stale exchange might forget.
        None => SCOPE.to_string(),
    };
    // The wanted scope can already be inside the base — the login set carries
    // `analytics.edit` now — and naming it twice in one consent request is
    // sloppy if nothing else.
    if base.split(' ').any(|s| s == wanted) {
        base
    } else {
        format!("{base} {wanted}")
    }
}

/// Block on the single redirect hit from the browser and pull `code` out of it.
/// One PKCE pair: the secret kept here and the digest sent to the provider.
pub(crate) struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// A fresh PKCE pair.
pub(crate) fn pkce() -> Pkce {
    let verifier: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

/// Random alphanumerics, for an OAuth `state` that has to be unguessable but
/// means nothing on its own.
pub(crate) fn nonce(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

/// The page the browser is left looking at once the redirect has come back.
///
/// An argument rather than a constant because the sentence differs by caller —
/// "connected to your Google Analytics account" is the wrong thing to say
/// about a Slack install — and, since `craft configure` moved behind the
/// subscription, because one of these pages has something to ask for.
#[derive(Clone, Copy)]
pub struct Landing<'a> {
    pub title: &'a str,
    pub body: &'a str,
    /// The one link this page is allowed to carry.
    ///
    /// A tab that has just handed over a permission is the cheapest place
    /// there will ever be to ask for the next thing: it is already open, it is
    /// the thing being looked at, and the terminal behind it is already
    /// sitting on a poll. Nothing else on the page is clickable, so when this
    /// is `Some` there is exactly one thing to do on it.
    pub cta: Option<Cta<'a>>,
}

/// A button, and the line under it that says what pressing it costs.
#[derive(Clone, Copy)]
pub struct Cta<'a> {
    pub label: &'a str,
    pub url: &'a str,
    pub note: &'a str,
}

impl<'a> Landing<'a> {
    /// A page with nothing to press — every landing but the paywall's.
    pub const fn plain(title: &'a str, body: &'a str) -> Landing<'a> {
        Landing {
            title,
            body,
            cta: None,
        }
    }
}

/// What `ensure_scope` leaves behind when there is nothing further to ask.
pub const GRANTED: Landing<'static> = Landing::plain(
    "Permission granted",
    "anacraft can set up the property now. \
     You can close this tab and return to the terminal.",
);

/// The browser tab, arrived on the redirect and not yet told how it went.
///
/// Held rather than answered on the spot so the page can depend on what the
/// sign-in turned out to be. The tab spins for the length of one token
/// exchange and one subscription lookup, which is the price of never showing
/// somebody an ask they have already paid.
///
/// Answering is not optional, and `Drop` is why: a caller that bails between
/// the redirect and the page — Google rejecting the exchange, a scope left
/// unticked — would otherwise leave the tab hanging on a request nobody ever
/// completes. It gets a plain page pointing back at the terminal, which is
/// true whichever way the run went.
#[derive(Debug)]
pub struct Tab {
    stream: Option<TcpStream>,
}

impl Tab {
    /// Answer the tab, and close it out.
    pub fn show(mut self, landing: &Landing<'_>) {
        if let Some(mut stream) = self.stream.take() {
            respond(&mut stream, &page(landing, Tone::Good));
        }
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            respond(
                &mut stream,
                &page(
                    &Landing::plain(
                        "Back to the terminal",
                        "The terminal has the rest of it — you can close this tab.",
                    ),
                    Tone::Good,
                ),
            );
        }
    }
}

/// The outcome of asking for a scope: either the credentials already carried
/// it, or a trip through the browser just granted it and there is a tab
/// waiting to be told what happened.
///
/// An enum rather than an `Option<Tab>` so the two cases read as what they are
/// at the call sites, all of which end in the same `show`.
pub enum Consented {
    /// Nothing was asked, because nothing needed asking. No tab, no page.
    AlreadyHeld,
    Granted(Tab),
}

impl Consented {
    /// Leave the browser looking at `landing`, if there is a browser to leave.
    pub fn show(self, landing: &Landing<'_>) {
        if let Consented::Granted(tab) = self {
            tab.show(landing);
        }
    }
}

/// Serve the loopback redirect until the provider hands over a code.
///
/// Hands back the tab alongside the code. What to say on it is the caller's
/// decision and, for `craft configure`, one it cannot make yet — see [`Tab`].
pub(crate) fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<(String, Tab)> {
    for stream in listener.incoming() {
        let mut stream = stream?;
        let request_line = {
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            reader.read_line(&mut line)?;
            line
        };

        // "GET /?code=...&state=... HTTP/1.1"
        let target = request_line.split_whitespace().nth(1).unwrap_or("/");
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);

        // Browsers often ask for /favicon.ico on the same port; ignore anything
        // that isn't the redirect we're waiting for.
        if params.is_empty() {
            respond(
                &mut stream,
                &page(
                    &Landing::plain("Waiting", "Nothing to see here yet."),
                    Tone::Good,
                ),
            );
            continue;
        }

        if let Some(err) = params.iter().find(|(k, _)| k == "error").map(|(_, v)| v) {
            respond(
                &mut stream,
                &page(
                    &Landing::plain(
                        "Login cancelled",
                        "Nothing was changed. You can close this tab.",
                    ),
                    Tone::Bad,
                ),
            );
            bail!("login cancelled: {err}");
        }

        let state = params
            .iter()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.as_str());
        if state != Some(expected_state) {
            respond(
                &mut stream,
                &page(
                    &Landing::plain(
                        "Rejected",
                        "The redirect did not match the request that started it. \
                         Close this tab and start again.",
                    ),
                    Tone::Bad,
                ),
            );
            bail!("OAuth state mismatch — login aborted");
        }

        let code = params
            .iter()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.clone())
            .context("no authorization code in redirect")?;

        return Ok((
            code,
            Tab {
                stream: Some(stream),
            },
        ));
    }
    bail!("browser never completed the login")
}

fn respond(stream: &mut TcpStream, html: &str) {
    let res = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    let _ = stream.write_all(res.as_bytes());
    let _ = stream.flush();
}

/// Whether the page is reporting success or a dead end. Only the accent
/// changes; the rest of the page is the same either way.
enum Tone {
    Good,
    Bad,
}

/// The 16x16 logo grid, the same one `scripts/gen-logo.py` draws the favicon
/// and the site mark from. Rows 0 and 15 are padding, so the SVG crops to the
/// glyph's own 14x14 extent.
const MARK: [&str; 16] = [
    "................",
    "......####......",
    ".....######.....",
    "....########....",
    "....########....",
    "...####..####...",
    "...####..####...",
    "..####....####..",
    "..####....####..",
    ".####......####.",
    ".##############.",
    ".##############.",
    ".####......####.",
    ".####......####.",
    ".####......####.",
    "................",
];

fn hex(color: ratatui::style::Color) -> String {
    match color {
        ratatui::style::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        // Every shipped palette is truecolor; this is only a safety net.
        _ => "#000000".to_string(),
    }
}

/// The mark as inline SVG rects, run-length encoded a row at a time so the
/// markup stays small enough to sit in a single response.
fn mark_svg(fill: &str) -> String {
    let mut out = String::from(
        "<svg viewBox=\"1 1 14 14\" width=\"52\" height=\"52\" \
         shape-rendering=\"crispEdges\" aria-hidden=\"true\">",
    );
    for (y, row) in MARK.iter().enumerate() {
        let cells: Vec<char> = row.chars().collect();
        let mut x = 0;
        while x < cells.len() {
            if cells[x] == '#' {
                let start = x;
                while x < cells.len() && cells[x] == '#' {
                    x += 1;
                }
                out.push_str(&format!(
                    "<rect x=\"{start}\" y=\"{y}\" width=\"{}\" height=\"1\" fill=\"{fill}\"/>",
                    x - start
                ));
            } else {
                x += 1;
            }
        }
    }
    out.push_str("</svg>");
    out
}

/// The browser page, drawn in whatever palette the user is running.
///
/// Deriving it from the live palette rather than hardcoding brand colours is
/// the whole point: this page cannot drift away from the dashboard the way the
/// previous hardcoded one did, and a light palette gets a readable light page
/// for free.
fn page(landing: &Landing<'_>, tone: Tone) -> String {
    let Landing { title, body, cta } = landing;
    let p = crate::theme::palette();
    let (ink, card, fg, dim, shadow) =
        (hex(p.ink), hex(p.bg), hex(p.fg), hex(p.sage), hex(p.shadow));
    let accent = match tone {
        Tone::Good => hex(p.accent),
        Tone::Bad => hex(p.coral),
    };
    let mark = mark_svg(&accent);

    // A strip of blocks under the card, in the ore vocabulary the dashboard
    // uses for a filled bar.
    let blocks: String = (0..14)
        .map(|i| {
            let c = if i < 11 { &accent } else { &shadow };
            format!("<i style=\"background:{c}\"></i>")
        })
        .collect();

    // The ask, when there is one. Painted in the accent on the accent's own
    // ink, so it reads as this page's own button rather than as a link
    // somebody dropped into it.
    let ask = match cta {
        Some(cta) => format!(
            "<a class=cta href=\"{}\">{}</a><p class=note>{}</p>",
            escape(cta.url),
            escape(cta.label),
            escape(cta.note),
        ),
        None => String::new(),
    };

    format!(
        "<!doctype html><html lang=en><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>anacraft — {title}</title>\
         <style>\
         *{{box-sizing:border-box}}\
         body{{background:{ink};color:{fg};margin:0;height:100vh;display:grid;\
         place-items:center;font-family:ui-monospace,'SF Mono',SFMono-Regular,Menlo,\
         Consolas,monospace;-webkit-font-smoothing:antialiased}}\
         .card{{background:{card};border:1px solid {shadow};border-top:3px solid {accent};\
         padding:44px 52px;text-align:center;max-width:min(92vw,460px);\
         animation:rise .28s ease-out both}}\
         svg{{display:block;margin:0 auto 22px}}\
         h1{{color:{accent};font-size:19px;font-weight:700;letter-spacing:.04em;\
         margin:0 0 10px}}\
         p{{color:{dim};font-size:13.5px;line-height:1.6;margin:0}}\
         .cta{{display:inline-block;margin-top:26px;padding:12px 26px;\
         background:{accent};color:{ink};text-decoration:none;font-weight:700;\
         font-size:12.5px;letter-spacing:.08em;text-transform:uppercase}}\
         .cta:hover{{opacity:.86}}\
         p.note{{margin-top:14px;font-size:12px}}\
         .bar{{display:flex;gap:2px;justify-content:center;margin-top:26px}}\
         .bar i{{width:9px;height:9px;display:block}}\
         @keyframes rise{{from{{opacity:0;transform:translateY(6px)}}}}\
         @media(prefers-reduced-motion:reduce){{.card{{animation:none}}}}\
         </style>\
         <div class=card>{mark}<h1>{title}</h1><p>{body}</p>{ask}\
         <div class=bar>{blocks}</div></div>"
    )
}

/// Escape for HTML, for the two places this page interpolates something that
/// is not a colour.
///
/// The paywall's button carries a checkout URL with a query string on it, and
/// a bare `&` in an attribute is the kind of thing that works in every browser
/// until the day it does not.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Percent-encode everything outside the unreserved set.
pub(crate) fn encode(s: &str) -> String {
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

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
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

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((decode(k), decode(v)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-in for whatever page a caller lands the browser on.
    const DONE: Landing<'static> = Landing::plain("Done", "Close this tab.");
    use std::net::TcpStream;
    use std::thread;

    #[test]
    fn the_page_is_drawn_from_the_active_palette() {
        // The old page hardcoded its colours, which is how it drifted away from
        // the dashboard. Pin the derivation so that cannot happen again.
        crate::theme::select("osaka-jade");
        let jade = page(&Landing::plain("Logged in", "body"), Tone::Good);
        assert!(jade.contains("#2dd5b7"), "accent missing");
        assert!(jade.contains("#09100d"), "ink missing");

        crate::theme::select("catppuccin-latte");
        let latte = page(&Landing::plain("Logged in", "body"), Tone::Good);
        assert!(
            !latte.contains("#09100d"),
            "a light palette must not paint the dark ground"
        );

        // The failure states differ only in the accent.
        crate::theme::select("osaka-jade");
        assert!(page(&Landing::plain("Rejected", "body"), Tone::Bad).contains("#ff5345"));

        crate::theme::select("osaka-jade");
    }

    #[test]
    fn a_tab_is_answered_even_when_nobody_chooses_a_page() {
        use std::io::Read as _;

        // The bail-out path: `consent` holds the tab open across the token
        // exchange, so every way out of that stretch — Google rejecting the
        // code, a scope left unticked, a subscription lookup that panics —
        // has to still leave the browser with a page. `Drop` is that promise,
        // and this is the test that it is kept.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();

        let browser = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            let mut body = String::new();
            let _ = stream.read_to_string(&mut body);
            body
        });

        let (server, _) = listener.accept().unwrap();
        drop(Tab {
            stream: Some(server),
        });

        let body = browser.join().unwrap();
        assert!(body.starts_with("HTTP/1.1 200 OK"), "got: {body}");
        assert!(body.contains("terminal"), "no way back named: {body}");
    }

    #[test]
    fn a_landing_with_nothing_to_ask_has_nothing_to_click() {
        // The ordinary login page must stay a dead end. A stray button on it
        // would be the one clickable thing in front of somebody who has just
        // been told they are signed in.
        let plain = page(&Landing::plain("Logged in", "body"), Tone::Good);
        assert!(!plain.contains("class=cta"), "an ask appeared unasked for");
        assert!(!plain.contains("<a "), "the plain page grew a link");
    }

    #[test]
    fn the_paywall_button_carries_an_escaped_checkout_url() {
        // The checkout URL has a query string, so the `&` between its
        // parameters has to survive the trip through an HTML attribute.
        let page = page(
            &Landing {
                title: "One thing left",
                body: "body",
                cta: Some(Cta {
                    label: "Become an Anacrafter",
                    url: "https://buy.stripe.com/x?client_reference_id=t&prefilled_email=a%40b.co",
                    note: "$2.99/month",
                }),
            },
            Tone::Good,
        );
        assert!(page.contains("client_reference_id=t&amp;prefilled_email=a%40b.co"));
        assert!(
            !page.contains("id=t&prefilled"),
            "an unescaped & reached the attribute"
        );
        assert!(page.contains("Become an Anacrafter"));
    }

    #[test]
    fn the_mark_is_cropped_to_its_glyph() {
        // Rows 0 and 15 are padding; emitting them would offset the logo inside
        // its own box.
        let svg = mark_svg("#000000");
        assert!(svg.contains(r#"viewBox="1 1 14 14""#));
        assert!(!svg.contains(r#"y="0""#), "padding row 0 was drawn");
        assert!(!svg.contains(r#"y="15""#), "padding row 15 was drawn");
        // Run-length encoding: the two solid crossbar rows are one rect each.
        assert_eq!(svg.matches(r#"width="14""#).count(), 2);
    }

    #[test]
    fn signing_in_asks_for_analytics_and_nothing_else() {
        // A Google OAuth review once stalled because the consent screen listed
        // `analytics` (read+write) and `analytics.manage.users.readonly`, which
        // this app has never requested. The identity scopes added for
        // subscriptions are the non-sensitive pair and need no review; pin the
        // whole set.
        assert_eq!(
            SCOPE,
            "openid email https://www.googleapis.com/auth/analytics.readonly \
             https://www.googleapis.com/auth/analytics.edit"
        );
        let analytics: Vec<&str> = SCOPE
            .split(' ')
            .filter(|s| s.contains("googleapis.com/auth/analytics"))
            .collect();
        // Read for the reports, edit for `craft configure` and `craft delete
        // --all` — the two Analytics scopes this binary uses, and no third one.
        assert_eq!(
            analytics.len(),
            2,
            "an Analytics scope was added or dropped"
        );
        for scope in SCOPE.split(' ') {
            assert!(
                matches!(scope, "openid" | "email") || scope.contains("/auth/analytics"),
                "unreviewed scope {scope} crept in"
            );
        }
    }

    #[test]
    fn the_write_scope_is_the_narrowest_one_that_creates_a_property() {
        // `analytics.edit` is what properties.create, dataStreams.create and
        // properties.delete document as their requirement. The neighbouring
        // scopes are all wider: `analytics` adds report data,
        // `analytics.manage.users` adds permission to change who can see the
        // account, and `analytics.provision` adds creating accounts and
        // accepting terms on someone's behalf. Requesting any of those would be
        // asking for access nothing here uses.
        assert_eq!(SCOPE_EDIT, "https://www.googleapis.com/auth/analytics.edit");
    }

    #[test]
    fn the_extra_scope_is_asked_for_on_top_of_the_granted_ones() {
        // Sending the new scope alone would work, and would quietly drop the
        // granted ones on the way through — the report commands would then 403
        // until the next `craft login`. The request has to name both.
        let mut old: Tokens = serde_json::from_str(
            r#"{"access_token":"a","refresh_token":"r","expires_at":"2030-01-01T00:00:00Z","scope":"openid email https://www.googleapis.com/auth/analytics.readonly"}"#,
        )
        .unwrap();
        let requested = extend_scopes(Some(&old), SCOPE_EDIT);
        assert!(requested.contains("analytics.readonly"));
        assert!(requested.contains("analytics.edit"));
        assert!(requested.starts_with("openid email"));

        // The login set now carries the edit scope as well, so topping it up
        // must not name the same scope twice.
        old.scope = Some(SCOPE.to_string());
        assert_eq!(extend_scopes(Some(&old), SCOPE_EDIT), SCOPE.to_string());

        // No recorded scopes is the worst case: fall back to the whole login
        // set so a stale exchange cannot quietly drop access.
        let requested = extend_scopes(None, SCOPE_EDIT);
        assert!(requested.contains("analytics.edit"));
        assert!(requested.contains("analytics.readonly"));
    }

    #[test]
    fn stored_credentials_report_which_scopes_they_carry() {
        let mut tokens: Tokens = serde_json::from_str(
            r#"{"access_token":"a","refresh_token":"r","expires_at":"2030-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        // Written before scopes were recorded: unknown reads as not granted,
        // which costs a consent screen rather than a 403.
        assert!(!tokens.granted(SCOPE_EDIT));

        // Read-only credentials — the login scope before the edit scope joined
        // it — must not satisfy the write check.
        tokens.scope =
            Some("openid email https://www.googleapis.com/auth/analytics.readonly".to_string());
        assert!(
            !tokens.granted(SCOPE_EDIT),
            "read-only must not imply write"
        );
        assert!(tokens.granted("https://www.googleapis.com/auth/analytics.readonly"));

        tokens.scope = Some(SCOPE.to_string());
        assert!(tokens.granted(SCOPE_EDIT));

        // Prefix matching would be a real bug here: `analytics.edit` must not
        // be satisfied by a scope that merely starts the same way.
        tokens.scope = Some("https://www.googleapis.com/auth/analytics.editors".to_string());
        assert!(!tokens.granted(SCOPE_EDIT));
    }

    #[test]
    fn the_account_comes_out_of_the_id_token() {
        // A real id token's middle segment: base64url, no padding.
        let payload =
            URL_SAFE_NO_PAD.encode(br#"{"sub":"110147","email":"me@example.com","aud":"x"}"#);
        let account = account_from_id_token(&format!("header.{payload}.signature")).unwrap();
        assert_eq!(account.sub, "110147");
        assert_eq!(account.email.as_deref(), Some("me@example.com"));
    }

    #[test]
    fn a_token_without_identity_is_no_identity_rather_than_a_failure() {
        // Google omits the id token when `openid` was never granted, and a
        // login from before the identity scopes has none stored. Neither is an
        // error: reports work regardless, only the cross-machine lookup needs
        // it.
        assert!(account_from_id_token("not-a-jwt").is_none());
        assert!(account_from_id_token("a.!!!!.c").is_none());
        let empty = URL_SAFE_NO_PAD.encode(br#"{"sub":""}"#);
        assert!(account_from_id_token(&format!("a.{empty}.c")).is_none());
        let no_sub = URL_SAFE_NO_PAD.encode(br#"{"email":"me@example.com"}"#);
        assert!(account_from_id_token(&format!("a.{no_sub}.c")).is_none());
    }

    #[test]
    fn stored_credentials_from_before_identity_still_load() {
        // token.json written by 0.7.x has no `account` key at all.
        let old = r#"{"access_token":"a","refresh_token":"r","expires_at":"2030-01-01T00:00:00Z"}"#;
        let tokens: Tokens = serde_json::from_str(old).unwrap();
        assert!(tokens.account.is_none());
    }

    #[test]
    fn encode_leaves_unreserved_characters_alone() {
        assert_eq!(encode("abcXYZ019-_.~"), "abcXYZ019-_.~");
    }

    #[test]
    fn encode_escapes_url_syntax() {
        assert_eq!(
            encode("https://www.googleapis.com/auth/analytics.readonly"),
            "https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fanalytics.readonly"
        );
    }

    #[test]
    fn decode_reverses_encode() {
        for original in [
            "http://127.0.0.1:8080",
            "4/0Ab_5qL-xyz+abc",
            "a b&c=d",
            "plain",
        ] {
            assert_eq!(decode(&encode(original)), original, "round trip failed");
        }
    }

    #[test]
    fn decode_survives_malformed_escapes() {
        // A trailing or invalid escape must not panic or truncate.
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(decode("a%2"), "a%2");
    }

    #[test]
    fn parse_query_splits_pairs() {
        let params = parse_query("code=abc123&state=xyz&scope=a%2Fb");
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], ("code".into(), "abc123".into()));
        assert_eq!(params[2], ("scope".into(), "a/b".into()));
        assert!(parse_query("").is_empty());
    }

    /// Fire a single bare HTTP request at the loopback listener.
    fn hit(port: u16, target: &str) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let _ = stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes());
        let _ = stream.flush();
    }

    #[test]
    fn captures_the_authorization_code() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || hit(port, "/?code=4%2FabcXYZ&state=secret"));

        let (code, tab) = wait_for_code(&listener, "secret").unwrap();
        assert_eq!(code, "4/abcXYZ");
        tab.show(&DONE);
    }

    #[test]
    fn ignores_favicon_before_the_real_redirect() {
        // Browsers routinely request /favicon.ico on the same port; that must
        // not be mistaken for the redirect and abort the login.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || {
            hit(port, "/favicon.ico");
            thread::sleep(std::time::Duration::from_millis(50));
            hit(port, "/?code=realcode&state=secret");
        });

        let (code, tab) = wait_for_code(&listener, "secret").unwrap();
        assert_eq!(code, "realcode");
        tab.show(&DONE);
    }

    #[test]
    fn rejects_a_mismatched_state() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || hit(port, "/?code=abc&state=attacker"));

        let err = wait_for_code(&listener, "secret").unwrap_err().to_string();
        assert!(err.contains("state mismatch"), "got: {err}");
    }

    #[test]
    fn surfaces_a_denied_consent() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        thread::spawn(move || hit(port, "/?error=access_denied&state=secret"));

        let err = wait_for_code(&listener, "secret").unwrap_err().to_string();
        assert!(err.contains("access_denied"), "got: {err}");
    }
}
