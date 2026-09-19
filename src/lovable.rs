//! `craft lovable` — point craft at the GA4 property a Lovable app already tags.
//!
//! What this replaces is a copy-paste. The setup on anacraft.dev's Lovable page
//! reads: tell Lovable "add Google Analytics", read a `G-XXXXXXX` off the
//! screen, run `craft props`, find that id in the list, `craft use` it. Four
//! steps, and the middle two exist only because the measurement id lives on one
//! screen and is needed on another.
//!
//! Lovable serves its projects over MCP, so the id does not have to travel by
//! hand: this reads the project's own source, finds the tag in it, and sets the
//! GA4 property that tag names as the default. The same trip `craft login` and
//! `craft use` already make, made once and without the eyeballing.
//!
//! **Read-only against Lovable.** It lists projects and reads files. It never
//! sends the project's agent a message, never writes project knowledge, never
//! deploys. Everything it changes is on this machine.
//!
//! **No client secret is embedded**, for the same reason `slack.rs` embeds
//! none: Lovable's token endpoint accepts `none` as an auth method, which makes
//! this a public client and PKCE the thing protecting the exchange. Unlike
//! Slack there is not even a client id to bake in — Lovable runs open dynamic
//! client registration for loopback redirects, so the binary registers itself
//! the first time somebody links and keeps what it was issued.
//!
//! The token that comes back is a credential: it can read every project in the
//! workspace. So it lands in `~/.anacraft/` at `0600` beside the Google one,
//! never in `config.toml`, which the README calls safe to commit to a dotfile
//! repo.

use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::{self, encode};
use crate::config::{self, Config};
use crate::configure;
use crate::ga::{self, Ga};
use crate::render::{bold, dim, paint};
use crate::theme::{glyph, ore};

/// Where the tools live. The OAuth endpoints below belong to `lovable.dev`
/// itself rather than to this host — the protected-resource metadata at
/// `{MCP_URL}/.well-known/oauth-protected-resource` is what names them, and
/// these constants are that answer written down rather than fetched twice per
/// run.
const MCP_URL: &str = "https://mcp.lovable.dev";

const AUTHORIZE_URL: &str = "https://lovable.dev/oauth/authorize";
const TOKEN_URL: &str = "https://lovable.dev/oauth/token";
const REGISTER_URL: &str = "https://lovable.dev/oauth/register";

/// What the link asks for, and nothing beyond it.
///
/// `projects:read` and `workspaces:read` are enough to list projects and read
/// their files, which is all this does. The write scopes Lovable offers —
/// `projects:write`, `workspaces:write`, `projects:create` — are deliberately
/// not requested: a consent screen that does not ask for them is a promise the
/// binary cannot quietly break later.
///
/// `offline` is what makes a refresh token come back, so linking once survives
/// the access token expiring.
const SCOPE: &str = "offline projects:read workspaces:read";

/// The MCP revision this *client* asks Lovable to speak.
///
/// Deliberately **not** [`crate::mcp::PROTOCOL_VERSION`]. That constant is the
/// revision `craft mcp` answers its own clients with; this one is what a
/// different server is asked for. They are equal today and there is no reason
/// they must stay equal — bumping one to chase Lovable would silently change
/// what our own server tells Claude Desktop. The test at the bottom is what
/// keeps someone from "tidying" the duplication away.
const PROTOCOL: &str = "2025-11-25";

/// An override for somebody who registered a client by hand, or who is
/// pointing a build at their own Lovable app. Same precedence idea as
/// `auth::ClientCreds::load`: the environment wins over what was saved.
const CLIENT_ID_ENV: &str = "ANACRAFT_LOVABLE_CLIENT_ID";

/// A workspace API key (`lov_…`), for a headless run that cannot open a
/// browser — CI, mostly. Env only: it is a credential, and a flag would put it
/// in shell history.
const API_KEY_ENV: &str = "ANACRAFT_LOVABLE_API_KEY";

// ------------------------------------------------------------------ record ---

/// What a link leaves behind: the grant, the registration it was made with,
/// and which project it points at.
///
/// One file, so `--unlink` is one delete. Every field added after the first
/// release carries `#[serde(default)]` — a record written by an older build
/// must still load, because the alternative is an upgrade that silently asks
/// somebody to sign in again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,

    /// The client id dynamic registration issued to this machine. Kept because
    /// the refresh grant has to present it again.
    pub client_id: String,
    /// Handed back by registration so the client can be deleted later. Absent
    /// when the id came from [`CLIENT_ID_ENV`] — that one is not ours to
    /// remove.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_client_uri: Option<String>,

    /// What was granted, which is not always what was asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Who Lovable says is signed in, for the status line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,

    /// The picked project. Absent in the window between signing in and picking
    /// one — the grant is saved as soon as it arrives, so a failure while
    /// choosing does not cost another trip through the browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    /// The workspace the project lives in. Kept because `list_projects` is
    /// per-workspace, so a later listing needs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Where Lovable publishes the project. This is what the domain route
    /// reconciles on when the measurement id is not in the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_url: Option<String>,

    pub linked_at: DateTime<Utc>,
}

impl Link {
    fn path() -> Result<PathBuf> {
        Ok(config::home()?.join("lovable.json"))
    }

    /// The saved link, or `None` if there is not one.
    ///
    /// An unreadable file reads as absent rather than as an error, the same
    /// way `slack::Install::load` treats a corrupt record: the only thing
    /// downstream does with this is decide whether to ask for a sign-in, and a
    /// damaged file should send somebody to `craft lovable --link`, not stop
    /// the command from running.
    pub fn load() -> Option<Link> {
        let raw = std::fs::read_to_string(Self::path().ok()?).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn save(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(self)?;
        config::write_private(&Self::path()?, &raw)
    }

    pub fn clear() -> Result<()> {
        let path = Self::path()?;
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        Ok(())
    }

    /// Refresh a minute early so a long scan cannot expire mid-flight. The
    /// same rule, and the same reason, as `auth::Tokens::is_stale`.
    pub fn is_stale(&self) -> bool {
        Utc::now() + Duration::seconds(60) >= self.expires_at
    }

    /// How to name the linked project out loud.
    pub fn project(&self) -> Option<String> {
        let id = self.project_id.as_ref()?;
        Some(match &self.project_name {
            Some(name) => format!("{name} ({id})"),
            None => id.clone(),
        })
    }
}

/// Where Lovable sends the code.
///
/// `127.0.0.1` rather than `localhost`: Lovable refuses the latter with
/// `invalid_request`. Open dynamic registration is documented as limited to
/// loopback, and this is the spelling of loopback it takes.
fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

// --------------------------------------------------------------- transport ---

/// How a request proves who it is.
///
/// Two ways in, because a browser is not always available: the OAuth grant a
/// `--link` leaves behind, or a workspace key from [`API_KEY_ENV`] for a
/// headless run.
#[derive(Clone)]
pub enum Credential {
    Bearer(String),
    ApiKey(String),
}

/// Redacted on purpose: this is a credential, and the whole point of keeping
/// it out of `config.toml` is undone by a struct that prints itself.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::Bearer(_) => f.write_str("Bearer(<redacted>)"),
            Credential::ApiKey(_) => f.write_str("ApiKey(<redacted>)"),
        }
    }
}

impl Credential {
    /// The header this credential travels in. Lovable names its own rather
    /// than overloading `Authorization`, so the two are not interchangeable.
    fn header(&self) -> (&'static str, String) {
        match self {
            Credential::Bearer(token) => ("Authorization", format!("Bearer {token}")),
            Credential::ApiKey(key) => ("Lovable-API-Key", key.clone()),
        }
    }
}

/// One JSON-RPC message out of a body that may be plain JSON or one SSE frame.
///
/// Which it is gets read off the content type **every time** rather than
/// decided once at connect: a server is allowed to answer the same request
/// either way, and a client that picked a parser at handshake time reads the
/// other shape as a protocol error.
///
/// An event stream may carry `:` comments, an `event:` line, and several
/// `data:` lines that concatenate — and it may carry a notification *before*
/// the reply, which is why this looks for a frame with `result` or `error` in
/// it instead of taking the first frame it can parse.
fn message(content_type: &str, body: &str) -> Result<Value> {
    if !content_type
        .to_ascii_lowercase()
        .contains("text/event-stream")
    {
        return serde_json::from_str(body)
            .with_context(|| format!("unexpected response shape from Lovable: {}", clip(body)));
    }

    let mut data = String::new();
    let mut frames = 0usize;

    // A trailing empty line flushes the last frame whether or not the body
    // ended with a blank line of its own.
    for line in body.lines().chain(std::iter::once("")) {
        let line = line.strip_suffix('\r').unwrap_or(line);

        if line.is_empty() {
            if !data.is_empty() {
                frames += 1;
                if let Ok(value) = serde_json::from_str::<Value>(&data) {
                    // A notification has neither, and is not the answer.
                    if value.get("result").is_some() || value.get("error").is_some() {
                        return Ok(value);
                    }
                }
                data.clear();
            }
            continue;
        }

        // Comments, and the framing fields this client has no use for.
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }

    bail!(
        "Lovable sent {frames} event stream frame(s) but no reply in any of them: {}",
        clip(body)
    )
}

/// Enough of a body to identify it in an error, and not enough to paste a
/// project's source into a terminal.
fn clip(body: &str) -> String {
    let trimmed = body.trim();
    match trimmed.char_indices().nth(200) {
        Some((cut, _)) => format!("{}…", &trimmed[..cut]),
        None => trimmed.to_string(),
    }
}

/// Turn a refusal into a sentence that names what to do about it.
///
/// Modelled on `ga::explain`: the caller has no better idea what a 403 means
/// than this does, and an HTTP status pasted into a terminal has never helped
/// anybody. Two layers matter here — Lovable can fail at the HTTP level, or
/// inside a 200 with a JSON-RPC `error`, and only the first reaches this.
fn explain(status: u16, body: &str) -> String {
    match status {
        401 => "the Lovable link has expired — run `craft lovable --link` again".to_string(),
        403 => "this Lovable account cannot read that project — \
                check you are signed in as the account that owns it"
            .to_string(),
        404 => {
            "that Lovable project is gone — run `craft lovable --link` to pick another".to_string()
        }
        429 => {
            "Lovable rate-limited this request; nothing was changed, try again shortly".to_string()
        }
        500..=599 => format!(
            "could not reach Lovable's MCP server ({status}) — \
             nothing was changed, try again shortly"
        ),
        _ => format!("Lovable refused the call ({status}): {}", clip(body)),
    }
}

/// The rows in a listing page.
///
/// The named key first, then the page itself if it is bare array, then the
/// only array in it. A renamed field should cost a guess rather than reading
/// silently as "you have nothing" — which is the shape of failure this tool
/// surface actually produces.
fn rows<'a>(page: &'a Value, key: &str) -> Option<&'a Vec<Value>> {
    if let Some(found) = page.get(key).and_then(Value::as_array) {
        return Some(found);
    }
    if let Some(found) = page.as_array() {
        return Some(found);
    }
    page.as_object()?.values().find_map(Value::as_array)
}

/// The cursor for the next page, if there is one.
///
/// Absent, null and empty all mean the same thing — the listing ended — and
/// treating an empty string as a cursor is how a pager loops forever.
fn next_cursor(page: &Value) -> Option<&str> {
    page.get("pagination")?
        .get("next_cursor")?
        .as_str()
        .filter(|c| !c.is_empty())
}

/// What a paged listing came back with, and whether the cap cut it short.
///
/// `truncated` travels as data rather than printing, because the caller may be
/// deciding rather than reporting — the same reason `configure::find_existing`
/// hands its note back instead of saying it.
pub struct Page {
    pub items: Vec<Value>,
    pub truncated: bool,
}

/// Stop conditions for a pager, so a pathological project cannot spin.
const MAX_PAGES: usize = 20;
const MAX_ITEMS: usize = 2000;

/// A connection to a remote MCP server.
pub struct Mcp {
    http: reqwest::Client,
    endpoint: String,
    auth: Credential,
    /// Learned from the `Mcp-Session-Id` response header at initialize, and
    /// echoed on every later request.
    session: Option<String>,
    /// The revision the server actually chose, which is what later requests
    /// declare. Taking the server's answer rather than repeating our ask is
    /// what makes this keep working when Lovable moves.
    version: String,
    next_id: u64,
}

impl Mcp {
    /// Shake hands, and come back ready to call tools.
    pub async fn connect(http: reqwest::Client, endpoint: &str, auth: Credential) -> Result<Mcp> {
        let mut mcp = Mcp {
            http,
            endpoint: endpoint.to_string(),
            auth,
            session: None,
            version: PROTOCOL.to_string(),
            next_id: 0,
        };

        let hello = mcp
            .send(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "anacraft",
                        "title": "anacraft",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
                true,
            )
            .await?;

        if let Some(agreed) = hello.get("protocolVersion").and_then(Value::as_str) {
            mcp.version = agreed.to_string();
        }

        // No id, so no reply is expected and none is waited for.
        let _ = mcp
            .send("notifications/initialized", json!({}), false)
            .await;

        Ok(mcp)
    }

    /// One JSON-RPC round trip, returning the `result`.
    async fn send(&mut self, method: &str, params: Value, expect_reply: bool) -> Result<Value> {
        self.next_id += 1;
        let mut payload = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        if expect_reply {
            payload["id"] = json!(self.next_id);
        }

        let (header, value) = self.auth.header();
        let mut request = self
            .http
            .post(&self.endpoint)
            .header(header, value)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Mcp-Protocol-Version", &self.version);

        if let Some(session) = &self.session {
            request = request.header("Mcp-Session-Id", session.clone());
        }

        let response = request
            .json(&payload)
            .send()
            .await
            .context("could not reach Lovable's MCP server")?;

        // Only worth learning once, and only the handshake offers it.
        if self.session.is_none() {
            if let Some(id) = response.headers().get("mcp-session-id") {
                self.session = id.to_str().ok().map(str::to_string);
            }
        }

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = response.text().await.unwrap_or_default();

        if !(200..300).contains(&status) {
            bail!("{}", explain(status, &body));
        }

        if !expect_reply {
            return Ok(Value::Null);
        }

        let envelope = message(&content_type, &body)?;

        // A 200 carrying a JSON-RPC error is still a refusal. Reading success
        // off the status line is the trap `slack.rs` records for Slack, and
        // JSON-RPC has the same shape.
        if let Some(error) = envelope.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let text = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no reason given");
            if code == -32601 {
                bail!(
                    "this Lovable account's MCP server does not offer `{method}`, \
                     which is how anacraft finds the tag.\n  \
                     The measurement id is on the Lovable screen that set analytics up — \
                     `craft use <id>` still works by hand."
                );
            }
            bail!("Lovable refused the call: {text}");
        }

        Ok(envelope.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Call one tool and unwrap what it answered.
    pub async fn tool(&mut self, name: &str, args: Value) -> Result<Value> {
        let result = self
            .send(
                "tools/call",
                json!({ "name": name, "arguments": args }),
                true,
            )
            .await?;
        unwrap_tool(name, result)
    }

    /// Walk a cursor-paginated tool to the end, or to the cap.
    pub async fn page_all(&mut self, name: &str, args: Value, key: &str) -> Result<Page> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let mut page_args = args.clone();
            if let Some(c) = &cursor {
                page_args["cursor"] = json!(c);
            }

            let page = self.tool(name, page_args).await?;
            if let Some(batch) = rows(&page, key) {
                items.extend(batch.iter().cloned());
            }

            if items.len() >= MAX_ITEMS {
                items.truncate(MAX_ITEMS);
                return Ok(Page {
                    items,
                    truncated: true,
                });
            }

            match next_cursor(&page) {
                Some(c) => cursor = Some(c.to_string()),
                None => {
                    return Ok(Page {
                        items,
                        truncated: false,
                    })
                }
            }
        }

        Ok(Page {
            items,
            truncated: true,
        })
    }
}

/// Unwrap the envelope a `tools/call` answers with.
///
/// The shape is the one `mcp.rs` builds for our own server: a `content` array
/// of text blocks, an optional `structuredContent`, and `isError`. The text
/// block usually carries JSON as a string, so the structured field is
/// preferred where it exists and the text is parsed where it is not.
fn unwrap_tool(name: &str, result: Value) -> Result<Value> {
    let text = || {
        result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|blocks| blocks.first())
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str)
    };

    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        bail!(
            "Lovable's `{name}` failed: {}",
            text().unwrap_or("no reason given")
        );
    }

    if let Some(structured) = result.get("structuredContent") {
        return Ok(structured.clone());
    }

    match text() {
        Some(raw) => Ok(serde_json::from_str(raw).unwrap_or_else(|_| json!(raw))),
        None => Ok(result),
    }
}

// ------------------------------------------------------------------- oauth ---

/// What dynamic registration hands back.
///
/// No `client_secret`: the request asks for `token_endpoint_auth_method:
/// "none"`, which is what makes this a public client and PKCE the thing
/// protecting the exchange.
#[derive(Debug, Deserialize)]
struct Registration {
    client_id: String,
    #[serde(default)]
    registration_access_token: Option<String>,
    #[serde(default)]
    registration_client_uri: Option<String>,
}

/// Lovable's token endpoint response.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
    /// What was granted, which is not always what was asked for.
    #[serde(default)]
    scope: Option<String>,
}

/// Register this machine as a client, the first time anybody links.
///
/// Lovable runs open dynamic registration for loopback redirects — the refusal
/// for anything else says so in as many words — which is why no client id has
/// to be baked into the binary the way Slack's is. A build with nothing
/// configured still works, which is what `CONTRIBUTING.md` promises.
///
/// The endpoint is a parameter so this can be pointed at a local server in a
/// test.
async fn register(
    http: &reqwest::Client,
    register_url: &str,
    redirect: &str,
) -> Result<Registration> {
    let response = http
        .post(register_url)
        .json(&json!({
            "client_name": "anacraft",
            "client_uri": "https://anacraft.dev",
            "redirect_uris": [redirect],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "application_type": "native",
            "scope": SCOPE,
        }))
        .send()
        .await
        .context("could not reach Lovable to register this machine")?;

    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();

    if !(200..300).contains(&status) {
        bail!(
            "Lovable would not register this machine ({status}): {}\n  \
             If you have a workspace API key, set {API_KEY_ENV}=lov_… and try again.",
            clip(&body)
        );
    }

    serde_json::from_str(&body).with_context(|| {
        format!(
            "unexpected registration response from Lovable: {}",
            clip(&body)
        )
    })
}

/// Trade the authorization code, or a refresh token, for an access token.
///
/// One function for both grants because Lovable takes them at the same
/// endpoint with the same public-client rules: no secret, and `client_id` in
/// the form. The endpoint is a parameter so a test can answer it.
async fn token(
    http: &reqwest::Client,
    token_url: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse> {
    let response = http
        .post(token_url)
        .form(form)
        .send()
        .await
        .context("could not reach Lovable to finish signing in")?;

    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();

    if !(200..300).contains(&status) {
        // `invalid_client` here means the registration this machine holds is
        // gone from Lovable's side — deleted, or expired. That is a different
        // fix from a rejected code, so it gets a different sentence.
        if body.contains("invalid_client") {
            bail!(
                "Lovable no longer recognises this machine's registration — \
                 run `craft lovable --link` to register again"
            );
        }
        bail!("Lovable rejected the sign-in ({status}): {}", clip(&body));
    }

    serde_json::from_str(&body)
        .with_context(|| format!("unexpected token response from Lovable: {}", clip(&body)))
}

/// How to authenticate as `link`, refreshing the grant first if it is close to
/// expiring, and saving the result.
///
/// Mirrors `auth::Auth::access_token`: the caller should not have to know
/// whether a refresh happened.
pub async fn credential(http: &reqwest::Client, link: &mut Link) -> Result<Credential> {
    // A workspace key travels in Lovable's own header, not as a bearer token.
    // Handing it back as a bare string is how it ends up in the wrong one.
    if let Ok(key) = std::env::var(API_KEY_ENV) {
        if !key.trim().is_empty() {
            return Ok(Credential::ApiKey(key));
        }
    }

    if !link.is_stale() {
        return Ok(Credential::Bearer(link.access_token.clone()));
    }

    let body = token(
        http,
        TOKEN_URL,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &link.refresh_token),
            ("client_id", &link.client_id),
        ],
    )
    .await?;

    link.access_token = body.access_token.clone();
    link.expires_at = Utc::now() + Duration::seconds(body.expires_in);
    // A rotated refresh token replaces the old one; an absent one means the
    // old one still stands. Dropping it either way would cost a re-link.
    if let Some(rotated) = body.refresh_token {
        link.refresh_token = rotated;
    }
    link.save()?;

    Ok(Credential::Bearer(body.access_token))
}

/// How this machine identifies itself, and whether that identity is ours to
/// delete later.
///
/// Env wins, so somebody can point a build at a client they registered by
/// hand; then whatever a previous link was issued, because a registration is
/// worth keeping — Lovable honours a loopback port change, so a cached id
/// still works on a different port and re-registering every time would leave a
/// trail of dead clients in the user's account; then a fresh registration.
async fn client_for(http: &reqwest::Client, redirect: &str) -> Result<Registration> {
    if let Ok(id) = std::env::var(CLIENT_ID_ENV) {
        if !id.trim().is_empty() {
            return Ok(Registration {
                client_id: id,
                registration_access_token: None,
                registration_client_uri: None,
            });
        }
    }

    if let Some(saved) = Link::load() {
        return Ok(Registration {
            client_id: saved.client_id,
            registration_access_token: saved.registration_access_token,
            registration_client_uri: saved.registration_client_uri,
        });
    }

    register(http, REGISTER_URL, redirect).await
}

/// Sign in to Lovable and keep the grant.
///
/// Comes back with the browser tab still open, the way `auth::consent` does:
/// what to leave a person looking at depends on what happens next — picking a
/// project can still fail — and a page that said "Linked" before the link was
/// usable would be a page that lied on the one occasion it mattered.
pub async fn sign_in(http: &reqwest::Client) -> Result<(Link, auth::Tab)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .context("could not open a local port for the Lovable redirect")?;
    let port = listener.local_addr()?.port();
    let redirect = redirect_uri(port);

    let registration = client_for(http, &redirect).await?;

    let auth::Pkce {
        verifier,
        challenge,
    } = auth::pkce();
    // Lovable refuses a short `state` outright with `invalid_state`, so this
    // length is load-bearing rather than merely prudent.
    let state = auth::nonce(24);

    let url = format!(
        "{AUTHORIZE_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}\
         &code_challenge={}&code_challenge_method=S256&state={}&resource={}",
        encode(&registration.client_id),
        encode(&redirect),
        encode(SCOPE),
        encode(&challenge),
        encode(&state),
        // Which resource the token is for. The MCP authorization spec asks for
        // it, and Lovable publishes the value in its protected-resource
        // metadata.
        encode(MCP_URL),
    );

    println!(
        "\n  {} opening your browser to sign in to Lovable…",
        paint(glyph::PICKAXE, ore::gold())
    );
    println!("  {}\n", dim("anacraft asks only to read your projects"));
    println!("  {}\n\n  {url}\n", dim("if it doesn't open, paste this:"));
    let _ = open::that(&url);

    let (code, tab) = auth::wait_for_code(&listener, &state)?;

    let body = token(
        http,
        TOKEN_URL,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect),
            ("client_id", &registration.client_id),
        ],
    )
    .await?;

    let refresh_token = body.refresh_token.context(
        "Lovable signed anacraft in but sent no refresh token — \
         was the `offline` scope approved?",
    )?;

    let link = Link {
        access_token: body.access_token,
        refresh_token,
        expires_at: Utc::now() + Duration::seconds(body.expires_in),
        client_id: registration.client_id,
        registration_access_token: registration.registration_access_token,
        registration_client_uri: registration.registration_client_uri,
        scope: body.scope,
        account: None,
        project_id: None,
        project_name: None,
        workspace_id: None,
        project_url: None,
        linked_at: Utc::now(),
    };
    // Saved before a project is picked, so a failure while choosing does not
    // cost another trip through the browser.
    link.save()?;

    Ok((link, tab))
}

/// Give the registration back to Lovable, so `--unlink` leaves nothing behind.
///
/// Best-effort: the local record goes either way. A client this build did not
/// register — one named by [`CLIENT_ID_ENV`] — is not ours to delete, and has
/// no registration token to delete it with.
async fn deregister(http: &reqwest::Client, link: &Link) -> bool {
    let (Some(uri), Some(token)) = (
        &link.registration_client_uri,
        &link.registration_access_token,
    ) else {
        return false;
    };

    http.delete(uri)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

// --------------------------------------------------------------- discovery ---

/// At most this many files are fetched, however big the project is. Discovery
/// is meant to read the handful of places a tag lives, not to download a repo.
const READ_CAP: usize = 12;

/// What an audit will open. Wider than [`READ_CAP`] because the questions are
/// wider — "is a view sent when the route changes" has to look at the routing,
/// not only at wherever the tag is.
const AUDIT_CAP: usize = 40;

/// One Lovable project, in the two facts this needs: what to call it, and
/// where it is published.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Project {
    pub id: String,
    pub name: Option<String>,
    /// The workspace this was listed from. `list_projects` is per-workspace,
    /// so this is always known by the time a project exists.
    pub workspace: Option<String>,
    pub workspace_name: Option<String>,
    /// Where Lovable publishes it. The domain route reconciles on this when
    /// the measurement id is not in the source, so its absence is what makes
    /// `--domain` necessary rather than optional.
    pub url: Option<String>,
}

impl Project {
    /// Read a project out of whatever shape the tool answered with.
    ///
    /// Tolerant on purpose: `mcp.lovable.dev` is young, and a renamed field
    /// should cost the domain route rather than the whole command. Each fact
    /// is looked for under the names it plausibly travels as.
    fn from_value(value: &Value) -> Option<Project> {
        let pick = |keys: &[&str]| -> Option<String> {
            keys.iter()
                .find_map(|k| value.get(*k).and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };

        Some(Project {
            id: pick(&["id", "project_id", "uuid"])?,
            name: pick(&["name", "title", "project_name"]),
            // Filled in by the caller, which is the thing that knows.
            workspace: None,
            workspace_name: None,
            url: pick(&[
                "url",
                "deployed_url",
                "published_url",
                "preview_url",
                "live_url",
                "domain",
            ]),
        })
    }

    pub fn display(&self) -> String {
        let named = match &self.name {
            Some(name) => format!("{name} ({})", self.id),
            None => self.id.clone(),
        };
        match &self.workspace_name {
            Some(workspace) => format!("{workspace} / {named}"),
            None => named,
        }
    }
}

/// Which project to work on.
///
/// No prompt: this binary never reads stdin, and the house answer to an
/// ambiguous choice is `configure::pick_account`'s — take the only one, take
/// the named one, or print the list and say the exact command to re-run.
fn pick<'a>(projects: &'a [Project], wanted: Option<&str>) -> Result<&'a Project> {
    if let Some(wanted) = wanted {
        let needle = wanted.trim().to_lowercase();
        return projects
            .iter()
            .find(|p| {
                p.id.to_lowercase() == needle
                    || p.name
                        .as_deref()
                        .is_some_and(|n| n.to_lowercase() == needle)
            })
            .with_context(|| format!("no Lovable project called {wanted}"));
    }

    match projects {
        [] => bail!(
            "this Lovable account has no projects.\n  \
             Create one at https://lovable.dev, then run this again."
        ),
        [only] => Ok(only),
        many => {
            let mut lines = String::new();
            for p in many {
                lines.push_str(&format!("\n    {}", p.display()));
            }
            bail!(
                "this Lovable account has {} projects:{lines}\n\n  \
                 then: craft lovable --link --project <id>",
                many.len()
            )
        }
    }
}

/// Paths a tag is never in, and paths that are none of our business.
fn skip(path: &str) -> bool {
    let lower = path.to_lowercase();

    // `.env` and its per-environment siblings hold the user's secrets.
    // `projects:read` is enough to fetch them and anacraft has no business
    // doing so; `.env.example` is the placeholder file and is safe.
    if lower.contains(".env") && !lower.ends_with(".env.example") {
        return true;
    }

    const NOISE: &[&str] = &[
        "node_modules/",
        "dist/",
        "build/",
        ".git/",
        "coverage/",
        ".next/",
        "supabase/migrations/",
    ];
    if NOISE
        .iter()
        .any(|d| lower.starts_with(d) || lower.contains(&format!("/{d}")))
    {
        return true;
    }

    const NOT_CODE: &[&str] = &[
        ".lock", ".md", ".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp", ".ico", ".woff",
        ".woff2", ".ttf", ".pdf", ".zip", ".map",
    ];
    NOT_CODE.iter().any(|ext| lower.ends_with(ext)) || lower.ends_with("package-lock.json")
}

/// How promising a path is, lowest first. `None` means do not read it at all.
///
/// The shape this has to survive is that there is no single Lovable project
/// layout. The documented one is Vite + React with `index.html` at the root
/// and `src/main.tsx` beside it; a newer template has no `.html` file at all
/// and routes from `src/routes/`. Ranking against one of those and refusing to
/// open anything else is how a scan reads seventy-six files as nothing.
///
/// So the exact entry points come first where they exist, and anything that is
/// plainly source comes after — bounded by [`READ_CAP`] rather than by a
/// guess about the framework.
fn score(path: &str) -> Option<u8> {
    if skip(path) {
        return None;
    }
    let lower = path.to_lowercase();
    let root = !lower.contains('/');
    let file = lower.rsplit('/').next().unwrap_or(&lower);
    let stem = file.split('.').next().unwrap_or(file);

    // A tag lives in HTML wherever the HTML is.
    if lower == "index.html" {
        return Some(0);
    }
    if lower.ends_with(".html") {
        return Some(1);
    }

    // The entry points a framework actually boots through, under whichever
    // name this template uses for them.
    let entry = matches!(
        stem,
        "main" | "app" | "root" | "__root" | "index" | "layout" | "document"
    );
    let in_src = lower.starts_with("src/");
    let codeish = [".ts", ".tsx", ".js", ".jsx", ".vue", ".svelte"]
        .iter()
        .any(|e| lower.ends_with(e));

    if entry && codeish && (in_src || root) {
        return Some(2);
    }

    // A filename that says what it is. Checked before the noise rule below,
    // so an analytics helper parked among the UI components is still read.
    if codeish
        && [
            "analytics",
            "gtag",
            "ga4",
            "tracking",
            "telemetry",
            "seo",
            "head",
            "metrics",
        ]
        .iter()
        .any(|w| lower.contains(w))
    {
        return Some(3);
    }

    if file == ".env.example" {
        return Some(4);
    }

    // Build config can inject a tag into the page at build time.
    if root && codeish && stem.ends_with("config") {
        return Some(5);
    }

    // Generated component libraries are dozens of files that never carry a
    // measurement id, and they would otherwise crowd out everything else.
    if lower.starts_with("src/components/ui/") {
        return None;
    }

    // Anything else that is plainly source, shallowest first, so a project
    // shaped in a way nobody anticipated is still looked at.
    if in_src && codeish {
        let depth = lower.matches('/').count();
        return Some(if depth <= 2 { 6 } else { 7 });
    }

    None
}

/// The files worth opening, best first, capped.
fn rank(paths: &[String]) -> Vec<String> {
    rank_within(paths, READ_CAP)
}

/// The same ordering on a stated budget.
///
/// Finding one measurement id and auditing the project are different
/// questions: the first stops as soon as it has looked in the likely places,
/// and the second would rather pay for breadth. Two callers, two budgets, one
/// ordering.
fn rank_within(paths: &[String], cap: usize) -> Vec<String> {
    let mut scored: Vec<(u8, &String)> = paths
        .iter()
        .filter_map(|p| score(p).map(|s| (s, p)))
        .collect();
    // Stable within a score so the order is the listing's, not the hash's.
    scored.sort_by_key(|(s, _)| *s);
    scored
        .into_iter()
        .take(cap)
        .map(|(_, p)| p.clone())
        .collect()
}

/// A placeholder is not a measurement id.
///
/// `G-XXXXXXXXXX` is the one that matters: it is printed by this project's own
/// setup guide, so it is very likely sitting in somebody's project inside a
/// comment. Treating it as real would point `craft` at nothing.
fn placeholder(tail: &str) -> bool {
    tail.chars().all(|c| c == tail.as_bytes()[0] as char)
        || matches!(tail, "XXXXXXXXXX" | "XXXXXXX" | "1234567890" | "ABCDEFGHIJ")
}

/// Every distinct GA4 measurement id in a file, in the order they appear.
///
/// Hand-rolled rather than a regex: the crate would be a real cost against a
/// release profile tuned with `opt-level = "z"` and LTO, for one pattern.
///
/// The rules that matter are what it *rejects*. A `G-` preceded by a letter or
/// digit is the tail of something else — `AG-`, and the `TM-` inside a Google
/// Tag Manager id — and a tail that is not uppercase-and-digits is not an id
/// at all, which is what keeps `g-something` and prose out.
pub(crate) fn measurement_ids(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut from = 0usize;

    while let Some(offset) = text[from..].find("G-") {
        let start = from + offset;
        let before_is_word = start > 0 && bytes[start - 1].is_ascii_alphanumeric();

        let tail_start = start + 2;
        let mut end = tail_start;
        while end < bytes.len() && (bytes[end].is_ascii_uppercase() || bytes[end].is_ascii_digit())
        {
            end += 1;
        }
        let tail = &text[tail_start..end];

        // Real ids are ten characters. The range is tolerant without being
        // credulous — it still refuses `G-` followed by a single letter.
        if !before_is_word && (7..=12).contains(&tail.len()) && !placeholder(tail) {
            let id = format!("G-{tail}");
            if !out.contains(&id) {
                out.push(id);
            }
        }

        from = tail_start;
    }

    out
}

/// Evidence that a file wires up Google Analytics without naming the id.
///
/// This is what Lovable's own Google Analytics connector leaves behind: it
/// exposes the measurement id to the frontend as an environment variable, so
/// the source loads gtag and reads `import.meta.env.…` and the `G-` is
/// nowhere. That is a working install, not a broken one, and telling somebody
/// their project has no analytics because of it would be wrong.
///
/// Returns the most specific token found, which is what gets quoted back.
pub(crate) fn wiring(text: &str) -> Option<String> {
    // The env var itself is the most useful thing to name, because it tells
    // the user exactly where Lovable is keeping the id.
    for marker in ["import.meta.env.", "process.env."] {
        let mut from = 0usize;
        while let Some(offset) = text[from..].find(marker) {
            let start = from + offset;
            let rest = &text[start + marker.len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let upper = name.to_uppercase();
            if !name.is_empty()
                && ["GA", "ANALYTICS", "MEASUREMENT", "GTAG"]
                    .iter()
                    .any(|w| upper.contains(w))
            {
                return Some(format!("{marker}{name}"));
            }
            from = start + marker.len();
        }
    }

    // A bare key, which is how `.env.example` carries it.
    for line in text.lines() {
        let key = line.trim().trim_start_matches('#').trim();
        let key = key.split('=').next().unwrap_or("").trim();
        let upper = key.to_uppercase();
        if (upper.starts_with("VITE_") || upper.starts_with("NEXT_PUBLIC_"))
            && ["GA_", "GA4", "ANALYTICS", "MEASUREMENT"]
                .iter()
                .any(|w| upper.contains(w))
        {
            return Some(key.to_string());
        }
    }

    for marker in [
        "googletagmanager.com/gtag/js",
        "react-ga4",
        "window.dataLayer",
        "gtag(",
    ] {
        if text.contains(marker) {
            return Some(marker.to_string());
        }
    }

    None
}

/// One measurement id, and the file it was found in.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub path: String,
    pub id: String,
}

/// One file that wires analytics up without naming an id.
#[derive(Debug, Clone, PartialEq)]
pub struct Wire {
    pub path: String,
    pub token: String,
}

/// What reading the project turned up.
#[derive(Debug, Default)]
pub struct Scan {
    pub hits: Vec<Hit>,
    pub wired: Vec<Wire>,
    /// How many files were opened, and how many the listing offered — both
    /// reported, so "nothing found" can say how hard it looked.
    pub read: usize,
    pub listed: usize,
    /// Files that were chosen and then would not open. Counted rather than
    /// swallowed: "read 0 of 76" means something very different depending on
    /// whether nothing was worth opening or nothing could be.
    pub failed: usize,
}

/// The conclusion, which is the thing worth testing.
#[derive(Debug, PartialEq)]
pub enum Found {
    /// One distinct id. `paths` may hold more than one file, and that is worth
    /// saying out loud rather than quietly picking the first.
    One {
        id: String,
        paths: Vec<String>,
    },
    /// Two or more distinct ids. Never guessed between.
    Many(Vec<Hit>),
    /// gtag is wired up but the id lives in a Lovable environment variable.
    Wired(Vec<Wire>),
    None,
}

fn decide(scan: &Scan) -> Found {
    let mut distinct: Vec<&str> = Vec::new();
    for hit in &scan.hits {
        if !distinct.contains(&hit.id.as_str()) {
            distinct.push(&hit.id);
        }
    }

    match distinct.len() {
        0 if scan.wired.is_empty() => Found::None,
        0 => Found::Wired(scan.wired.clone()),
        1 => Found::One {
            id: distinct[0].to_string(),
            paths: scan
                .hits
                .iter()
                .filter(|h| h.id == distinct[0])
                .map(|h| h.path.clone())
                .collect(),
        },
        _ => Found::Many(scan.hits.clone()),
    }
}

// ---------------------------------------------------------- reconciliation ---

/// Which GA4 property carries `measurement_id`, and anything the scan should
/// say out loud.
///
/// Deliberately the same shape as `configure::find_existing` — the same loop,
/// the same `SCAN_LIMIT`, the same `(answer, note)` return — because it is the
/// same question asked of a different field. That one matches on the stream's
/// `default_uri`; this one on the id the site actually sends to.
///
/// Properties already in the config are checked first, so the common re-run
/// costs one Admin call rather than forty.
async fn property_for_tag(
    ga: &Ga,
    cfg: &Config,
    measurement_id: &str,
) -> Result<(Option<(ga::Property, ga::WebStream)>, Option<String>)> {
    let mut properties = ga.properties().await?;
    let known: Vec<&str> = cfg.properties.iter().map(|p| p.id.as_str()).collect();
    properties.sort_by_key(|p| !known.contains(&p.id.as_str()));

    let total = properties.len();

    for property in properties.into_iter().take(configure::SCAN_LIMIT) {
        // A property this login cannot read streams on is not a match; it is
        // also not a reason to fail, so skip it.
        let Ok(streams) = ga.web_streams(&property.id).await else {
            continue;
        };

        if let Some(stream) = streams.iter().find(|s| s.measurement_id == measurement_id) {
            let found = ga::WebStream {
                name: stream.name.clone(),
                measurement_id: stream.measurement_id.clone(),
                default_uri: stream.default_uri.clone(),
            };
            return Ok((Some((property, found)), None));
        }
    }

    // An account larger than the scan is worth knowing about: the tag could be
    // on a property just past the limit, and "not found" would be wrong.
    let note = (total > configure::SCAN_LIMIT).then(|| {
        format!(
            "checked the first {} of {total} properties for {measurement_id}",
            configure::SCAN_LIMIT
        )
    });

    Ok((None, note))
}

/// Which route decides.
///
/// The measurement id wins wherever it resolves, because it is what the site
/// physically sends to — a stream's `default_uri` is free text somebody typed
/// into the console and GA4 never validates it.
///
/// The case worth care is the third: a tag was found in the source, but no
/// property this Google account can see carries it, while some property does
/// claim the domain. Quietly pointing `craft` at the domain match would make
/// every later number come from a property the site is not sending to. That is
/// a finding, not a fallback, so nothing is chosen automatically.
#[derive(Debug, PartialEq)]
enum Winner {
    /// The tag resolved. `disagrees` when the domain route found a different one.
    Tag { disagrees: bool },
    /// No tag in the source, but the project's domain is measured.
    Domain,
    /// A tag is on the site and this account cannot see the property for it.
    Stalemate,
    /// A tag is on the site, nothing resolves at all.
    TagUnseen,
    /// Nothing to go on. The mint branch.
    Nothing,
}

fn precedence(tag: Option<&str>, domain: Option<&str>, tagged: bool) -> Winner {
    match (tag, domain) {
        (Some(by_tag), by_domain) => Winner::Tag {
            disagrees: by_domain.is_some_and(|d| d != by_tag),
        },
        (None, Some(_)) if tagged => Winner::Stalemate,
        (None, Some(_)) => Winner::Domain,
        (None, None) if tagged => Winner::TagUnseen,
        (None, None) => Winner::Nothing,
    }
}

// ------------------------------------------------------------------- entry ---

/// What the command was asked to do.
#[derive(Debug, Default)]
pub struct Opts {
    pub project: Option<String>,
    pub id: Option<String>,
    pub git_ref: Option<String>,
    pub domain: Option<String>,
}

/// The paths a `list_files` page carried, however it shaped them.
///
/// Tolerant for the same reason [`Project::from_value`] is: a bare string and
/// an object with a `path` are both plausible, and guessing wrong should cost
/// a file rather than the command.
fn paths_in(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match item {
            Value::String(s) => Some(s.clone()),
            other => other
                .get("path")
                .or_else(|| other.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string),
        })
        .filter(|p| !p.is_empty())
        .collect()
}

/// The text a `read_file` answered with.
fn text_in(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        other => other
            .get("content")
            .or_else(|| other.get("text"))
            .or_else(|| other.get("contents"))
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// Read the project's likely files and report what is in them.
///
/// `list_files` for the tree, then [`rank`] to decide what is worth opening,
/// then at most [`READ_CAP`] reads. A project is never downloaded.
async fn scan_project(mcp: &mut Mcp, project: &str, git_ref: &str) -> Result<Scan> {
    let listing = mcp
        .page_all(
            "list_files",
            json!({ "project_id": project, "ref": git_ref }),
            "files",
        )
        .await?;

    if listing.truncated {
        println!(
            "  {}\n",
            dim("this project has more files than one scan reads; \
                 if the tag is missed, name it with --id")
        );
    }

    let paths = paths_in(&listing.items);
    let mut scan = Scan {
        listed: paths.len(),
        ..Default::default()
    };

    for path in rank(&paths) {
        let Ok(file) = mcp
            .tool(
                "read_file",
                json!({ "project_id": project, "path": path, "ref": git_ref }),
            )
            .await
        else {
            // One unreadable file is not a reason to abandon the scan, but it
            // is a reason to say so at the end.
            scan.failed += 1;
            continue;
        };
        let Some(text) = text_in(&file) else {
            scan.failed += 1;
            continue;
        };
        scan.read += 1;

        for id in measurement_ids(&text) {
            scan.hits.push(Hit {
                path: path.clone(),
                id,
            });
        }
        if let Some(token) = wiring(&text) {
            scan.wired.push(Wire {
                path: path.clone(),
                token,
            });
        }
    }

    Ok(scan)
}

/// Every project the account can see, and whether the listing was cut short.
///
/// `list_projects` requires a `workspace_id` — Lovable offers no "everything I
/// can see" call — so the workspaces are walked first and their projects
/// flattened into one list. That flattening is also what lets `--project` take
/// a single value without anybody having to name a workspace as well.
async fn projects_in_account(mcp: &mut Mcp) -> Result<(Vec<Project>, bool)> {
    let workspaces = mcp
        .page_all("list_workspaces", json!({}), "workspaces")
        .await?;

    let mut all = Vec::new();
    let mut truncated = workspaces.truncated;

    for workspace in &workspaces.items {
        let Some(id) = workspace
            .get("id")
            .or_else(|| workspace.get("workspace_id"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let name = workspace
            .get("name")
            .or_else(|| workspace.get("title"))
            .and_then(Value::as_str)
            .map(str::to_string);

        let listing = mcp
            .page_all("list_projects", json!({ "workspace_id": id }), "projects")
            .await?;
        truncated |= listing.truncated;

        for row in &listing.items {
            if let Some(mut project) = Project::from_value(row) {
                project.workspace = Some(id.to_string());
                project.workspace_name = name.clone();
                all.push(project);
            }
        }
    }

    Ok((all, truncated))
}

/// The linked project's source, for the audit to read.
///
/// `None` when nothing is linked. That is a reason for the source checks to
/// report as not run, never a reason for `craft audit` to fail — somebody
/// auditing an ordinary website has no Lovable project and is not doing
/// anything wrong.
///
/// Returns what to call the project alongside its files, so the report can say
/// whose code it read.
pub(crate) async fn source_files() -> Result<Option<(String, Vec<(String, String)>)>> {
    let Some(mut record) = Link::load() else {
        return Ok(None);
    };
    let Some(project_id) = record.project_id.clone() else {
        return Ok(None);
    };

    let http = reqwest::Client::new();
    let auth = credential(&http, &mut record).await?;
    let mut mcp = Mcp::connect(http, MCP_URL, auth).await?;

    let detail = mcp
        .tool("get_project", json!({ "project_id": &project_id }))
        .await?;
    let project = Project::from_value(&detail).unwrap_or_else(|| Project {
        id: project_id.clone(),
        name: record.project_name.clone(),
        ..Default::default()
    });
    let git_ref = detail
        .get("ref")
        .or_else(|| detail.get("default_branch"))
        .and_then(Value::as_str)
        .unwrap_or("main")
        .to_string();

    let listing = mcp
        .page_all(
            "list_files",
            json!({ "project_id": &project_id, "ref": &git_ref }),
            "files",
        )
        .await?;
    let listed = paths_in(&listing.items);

    // `package.json` is not a place a tag lives, so the tag scan skips it —
    // but it is the most reliable statement of which router the project uses,
    // and the routing checks are the ones worth being right about.
    let mut wanted = rank_within(&listed, AUDIT_CAP);
    if let Some(manifest) = listed.iter().find(|p| p.as_str() == "package.json") {
        wanted.insert(0, manifest.clone());
    }

    let mut files = Vec::new();
    for path in wanted {
        let Ok(answer) = mcp
            .tool(
                "read_file",
                json!({ "project_id": &project_id, "path": &path, "ref": &git_ref }),
            )
            .await
        else {
            continue;
        };
        if let Some(text) = text_in(&answer) {
            files.push((path, text));
        }
    }

    Ok(Some((project.display(), files)))
}

/// The plan that reading a Lovable project is part of.
///
/// Pro rather than Elite, and the same one `craft audit` asks for, because the
/// two are one feature: the source checks and the API checks are a single
/// report, and pricing half of it separately would put one command's findings
/// on two sides of a paywall. `craft mcp` stays Elite — serving the numbers to
/// an assistant is a different thing from reading a project.
///
/// `--unlink` and the bare status are deliberately outside this. Somebody
/// whose subscription has lapsed must still be able to see what is linked and
/// to take the credential off their machine.
async fn unlock() -> Result<()> {
    let cfg = config::Config::load().unwrap_or_default();
    let tier = crate::license::sync(&cfg).await;
    crate::license::gate(tier, crate::license::Tier::Pro, "craft lovable")
        .map_err(|reason| anyhow::anyhow!(reason))
}

/// Sign in to Lovable, pick a project, and reconcile it with GA4.
pub async fn link(opts: Opts) -> Result<()> {
    unlock().await?;

    let http = reqwest::Client::new();
    let (mut record, tab) = sign_in(&http).await?;

    let mut mcp = Mcp::connect(
        http.clone(),
        MCP_URL,
        Credential::Bearer(record.access_token.clone()),
    )
    .await?;

    let (projects, truncated) = projects_in_account(&mut mcp).await?;
    if truncated {
        println!(
            "  {}\n",
            dim("this account has more projects than one listing shows; \
                 name yours with --project if it is not below")
        );
    }
    // The tab is answered before the pick can fail, because by here the sign-in
    // itself has genuinely succeeded — which is what the page claims.
    tab.show(&auth::Landing::plain(
        "Linked",
        "anacraft can read your Lovable projects. \
         You can close this tab and return to the terminal.",
    ));

    let chosen = pick(&projects, opts.project.as_deref())?;
    record.project_id = Some(chosen.id.clone());
    record.project_name = chosen.name.clone();
    record.workspace_id = chosen.workspace.clone();
    record.project_url = chosen.url.clone();
    record.save()?;

    println!(
        "  {} {}  ·  {}\n",
        paint(glyph::STAR, ore::gold()),
        bold(&paint("linked", ore::emerald())),
        dim(&chosen.display())
    );

    reconcile(&mut mcp, &record, &opts).await
}

/// Re-read the linked project and reconcile it again.
pub async fn sync(opts: Opts) -> Result<()> {
    unlock().await?;

    let mut record = Link::load().context("not linked to Lovable — run `craft lovable --link`")?;

    let http = reqwest::Client::new();
    let auth = credential(&http, &mut record).await?;
    let mut mcp = Mcp::connect(http, MCP_URL, auth).await?;

    reconcile(&mut mcp, &record, &opts).await
}

/// The shared body: read the project, resolve the property, save the answer.
async fn reconcile(mcp: &mut Mcp, record: &Link, opts: &Opts) -> Result<()> {
    let project_id = record
        .project_id
        .as_deref()
        .context("no Lovable project picked yet — run `craft lovable --link`")?;

    let detail = mcp
        .tool("get_project", json!({ "project_id": project_id }))
        .await?;
    let project = Project::from_value(&detail).unwrap_or_else(|| Project {
        id: project_id.to_string(),
        name: record.project_name.clone(),
        workspace: record.workspace_id.clone(),
        workspace_name: None,
        url: record.project_url.clone(),
    });

    let git_ref = opts
        .git_ref
        .clone()
        .or_else(|| {
            detail
                .get("ref")
                .or_else(|| detail.get("default_branch"))
                .or_else(|| detail.get("branch"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "main".to_string());

    let scan = scan_project(mcp, project_id, &git_ref).await?;
    let found = decide(&scan);

    println!(
        "  {}\n",
        dim(&format!("{} at {git_ref}", project.display()))
    );
    report(&scan, &found);

    // `--id` is how somebody gets past a project carrying two tags.
    let chosen_id = match (&opts.id, &found) {
        (Some(id), _) => Some(id.clone()),
        (None, Found::One { id, .. }) => Some(id.clone()),
        (None, Found::Many(hits)) => {
            let mut lines = String::new();
            for hit in hits {
                lines.push_str(&format!("\n    {}  {}", hit.id, dim(&hit.path)));
            }
            bail!(
                "two or more GA4 measurement ids in this project:{lines}\n\n  \
                 A second GA4 tag doubles every number. Pick the one to follow:\n    \
                 craft lovable --sync --id {}\n  \
                 and take the other off the site.",
                hits[0].id
            );
        }
        (None, _) => None,
    };

    let host = opts.domain.clone().or_else(|| {
        project
            .url
            .as_deref()
            .and_then(|u| configure::host_of(u).ok())
    });

    // Nothing to look up means no reason to require a Google account. The
    // Lovable half has already said everything it knows.
    if chosen_id.is_none() && host.is_none() {
        return settle(Winner::Nothing, None, None, &found, None, None);
    }

    // A dev build with no OAuth client, or simply nobody signed in yet, must
    // not throw away the reading that already succeeded.
    let ga = match Ga::new() {
        Ok(ga) => ga,
        Err(why) => {
            println!(
                "  {} {}\n",
                paint(glyph::PICKAXE, ore::stone()),
                bold("stopping before the GA4 half")
            );
            println!("  {}\n", dim(&why.to_string().replace('\n', "\n  ")));
            return Ok(());
        }
    };
    let cfg = Config::load()?;

    let by_tag = match &chosen_id {
        Some(id) => {
            let (hit, note) = property_for_tag(&ga, &cfg, id).await?;
            if let Some(note) = note {
                println!("  {}\n", dim(&note));
            }
            hit
        }
        None => None,
    };

    // Only asked when the tag did not already answer: the second scan buys a
    // diagnosis rather than a duplicate answer.
    let host = opts.domain.clone().or_else(|| {
        project
            .url
            .as_deref()
            .and_then(|u| configure::host_of(u).ok())
    });
    let by_domain = match (&by_tag, &host) {
        (None, Some(host)) => match configure::find_existing(&ga, host).await?.0 {
            Some(configure::Existing::Measured(property, stream)) => Some((property, stream)),
            _ => None,
        },
        _ => None,
    };

    settle(
        precedence(
            by_tag.as_ref().map(|(p, _)| p.id.as_str()),
            by_domain.as_ref().map(|(p, _)| p.id.as_str()),
            chosen_id.is_some(),
        ),
        by_tag,
        by_domain,
        &found,
        chosen_id.as_deref(),
        host.as_deref(),
    )
}

/// Say what reading the project turned up, before anything is resolved.
///
/// This is the half that needs no Google account, and it is worth printing on
/// its own: somebody pointing craft at a Lovable project for the first time
/// wants to know what is in it, and whether the GA4 lookup then succeeds is a
/// separate question from whether the tag was found.
fn report(scan: &Scan, found: &Found) {
    println!(
        "  {}\n",
        dim(&format!(
            "read {} of {} files{}",
            scan.read,
            scan.listed,
            match scan.failed {
                0 => String::new(),
                n => format!(" · {n} would not open"),
            }
        ))
    );

    match found {
        Found::One { id, paths } => {
            println!(
                "  {} {}  ·  {}",
                paint(glyph::STAR, ore::gold()),
                bold(&paint(id, ore::emerald())),
                dim(&paths.join(", "))
            );
            if paths.len() > 1 {
                println!(
                    "  {}",
                    dim("the same tag in more than one place — \
                         a second GA4 tag doubles every number")
                );
            }
            println!();
        }
        Found::Many(hits) => {
            println!(
                "  {} {}",
                paint(glyph::PICKAXE, ore::redstone()),
                bold("more than one GA4 tag in this project")
            );
            for hit in hits {
                println!("    {}  {}", hit.id, dim(&hit.path));
            }
            println!();
        }
        Found::Wired(wires) => {
            println!(
                "  {} {}",
                paint(glyph::PICKAXE, ore::stone()),
                bold("analytics is wired up, but the id is not in the source")
            );
            for wire in wires {
                println!("    {}  {}", wire.token, dim(&wire.path));
            }
            println!(
                "  {}\n",
                dim("that is how Lovable's own Google Analytics connector \
                     installs it — the id is an environment variable")
            );
        }
        Found::None => {
            println!(
                "  {} {}\n",
                paint(glyph::PICKAXE, ore::stone()),
                bold("no GA4 tag and no analytics wiring in this project")
            );
        }
    }
}

/// Say what was decided, and save it when there is something to save.
fn settle(
    winner: Winner,
    by_tag: Option<(ga::Property, ga::WebStream)>,
    by_domain: Option<(ga::Property, ga::WebStream)>,
    found: &Found,
    measurement_id: Option<&str>,
    host: Option<&str>,
) -> Result<()> {
    let use_it = |property: &ga::Property, stream: &ga::WebStream, why: &str| -> Result<()> {
        let mut cfg = Config::load()?;
        cfg.upsert(&property.id, Some(property.name.clone()));
        cfg.active = Some(property.id.clone());
        cfg.save()?;

        println!(
            "  {} {}  ·  {}\n",
            paint(glyph::STAR, ore::gold()),
            bold(&paint(&property.name, ore::emerald())),
            dim(&format!("{} · {why}", stream.measurement_id))
        );
        println!(
            "  {}\n",
            dim("now the default property — `craft overview` or `craft dash`")
        );
        Ok(())
    };

    match winner {
        Winner::Tag { disagrees } => {
            let (property, stream) = by_tag.expect("a tag winner has a property");
            if disagrees {
                if let Some((other, _)) = &by_domain {
                    println!(
                        "  {} {}\n",
                        paint(glyph::PICKAXE, ore::redstone()),
                        dim(&format!(
                            "the tag names {} but {} also claims this domain — \
                             two properties on one site split every number; \
                             `craft audit` says which is collecting",
                            property.name, other.name
                        ))
                    );
                }
            }
            use_it(&property, &stream, "matched by the tag in the source")
        }
        Winner::Domain => {
            let (property, stream) = by_domain.expect("a domain winner has a property");
            if let Found::Wired(wires) = found {
                let where_ = wires
                    .first()
                    .map(|w| format!("{} reads {}", w.path, w.token))
                    .unwrap_or_default();
                println!(
                    "  {} {}\n",
                    paint(glyph::PICKAXE, ore::stone()),
                    dim(&format!(
                        "{where_}, so the measurement id is in a Lovable \
                         environment variable rather than the source — \
                         that is how Lovable's own Google Analytics connector installs it"
                    ))
                );
            }
            use_it(&property, &stream, "matched by domain, not by the tag")
        }
        Winner::Stalemate => {
            let (other, _) = by_domain.expect("a stalemate has a domain match");
            bail!(
                "{} is on this Lovable project, but no GA4 property this Google account \
                 can see carries it.\n  \
                 {} does claim {}, but pointing craft at it would report numbers the site \
                 never sends there.\n  \
                 Sign in as the Google account Lovable used — `craft login` — or \
                 `craft use {}` if that really is the right one.\n  \
                 The tag stays on the site either way. Nothing was changed.",
                measurement_id.unwrap_or("the tag"),
                other.name,
                host.unwrap_or("this domain"),
                other.id
            )
        }
        Winner::TagUnseen => bail!(
            "{} is on this Lovable project, but no GA4 property this Google account can see \
             carries it.\n  \
             Lovable may have made it under a different Google account — `craft login` as \
             that one — or you may need Viewer on the property.\n  \
             The tag stays on the site either way. Nothing was changed.",
            measurement_id.unwrap_or("the tag")
        ),
        Winner::Nothing => {
            println!(
                "  {} {}\n",
                paint(glyph::PICKAXE, ore::stone()),
                bold("no GA4 tag in this project yet")
            );
            match host {
                Some(host) => println!(
                    "  {}\n",
                    dim(&format!(
                        "`craft configure {host}` creates the property and prints the tag — \
                         then ask Lovable to put it in index.html's <head>"
                    ))
                ),
                None => println!(
                    "  {}\n",
                    dim(
                        "`craft lovable --sync --domain <yoursite.com>` once you know \
                         the domain, or `craft configure <domain>` directly"
                    )
                ),
            }
            println!(
                "  {}\n",
                dim("anacraft changed nothing in your Lovable project.")
            );
            Ok(())
        }
    }
}

/// Say what is linked, if anything.
pub fn status() -> Result<()> {
    match Link::load() {
        Some(record) => {
            println!(
                "\n  {} {}  ·  {}\n",
                paint(glyph::STAR, ore::gold()),
                bold(&paint("lovable", ore::emerald())),
                dim(&format!(
                    "{} · linked {}",
                    record
                        .project()
                        .unwrap_or_else(|| "no project picked".into()),
                    record.linked_at.format("%-d %b %Y")
                ))
            );
            println!("  {}\n", dim("`craft lovable --sync` re-reads it"));
        }
        None => {
            println!(
                "\n  {} {}\n\n  {}\n",
                paint(glyph::PICKAXE, ore::stone()),
                bold("not linked to Lovable"),
                dim("run `craft lovable --link` to point craft at a Lovable project")
            );
        }
    }
    Ok(())
}

/// Forget the link, and give the registration back.
pub async fn unlink() -> Result<()> {
    let Some(record) = Link::load() else {
        println!("\n  {}\n", dim("no Lovable link to remove"));
        return Ok(());
    };

    let removed = deregister(&reqwest::Client::new(), &record).await;
    Link::clear()?;

    println!(
        "\n  {} {}  ·  {}\n",
        paint(glyph::PICKAXE, ore::redstone()),
        bold("unlinked"),
        dim(&record.project().unwrap_or_else(|| "Lovable".into()))
    );
    println!(
        "  {}\n",
        dim(if removed {
            "the token is gone from this machine and the registration is gone from Lovable"
        } else {
            "the token is gone from this machine — revoke anacraft at lovable.dev \
             if you want the grant gone there too"
        })
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Raw requests the fake server saw, newest last.
    type Requests = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    fn link() -> Link {
        Link {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: Utc::now() + Duration::hours(1),
            client_id: "cid".into(),
            registration_access_token: Some("rat".into()),
            registration_client_uri: Some("https://lovable.dev/oauth/register/cid".into()),
            scope: Some(SCOPE.into()),
            account: Some("someone@example.com".into()),
            project_id: Some("p1".into()),
            project_name: Some("my-app".into()),
            workspace_id: Some("w1".into()),
            project_url: Some("https://my-app.lovable.app".into()),
            linked_at: Utc::now(),
        }
    }

    #[test]
    fn the_record_round_trips_and_keeps_the_grant() {
        let before = link();
        let after: Link = serde_json::from_str(&serde_json::to_string(&before).unwrap()).unwrap();

        assert_eq!(after.refresh_token, before.refresh_token);
        assert_eq!(after.client_id, before.client_id);
        assert_eq!(after.project_id, before.project_id);
    }

    #[test]
    fn a_link_made_before_a_project_was_picked_still_loads() {
        // The grant is saved the moment it arrives, which is before there is a
        // project to name. A record from that window must not be unreadable.
        let after: Link = serde_json::from_str(
            r#"{"access_token":"at","refresh_token":"rt",
                "expires_at":"2026-09-16T00:00:00Z","client_id":"cid",
                "linked_at":"2026-09-16T00:00:00Z"}"#,
        )
        .unwrap();

        assert!(after.project().is_none());
        assert!(after.registration_access_token.is_none());
    }

    #[test]
    fn a_grant_that_expires_within_the_minute_is_stale() {
        // A scan can make forty Admin API calls; a token that dies halfway
        // through is an error in the middle of the work rather than before it.
        let mut l = link();
        l.expires_at = Utc::now() + Duration::seconds(30);
        assert!(l.is_stale());

        l.expires_at = Utc::now() + Duration::seconds(300);
        assert!(!l.is_stale());
    }

    #[test]
    fn the_scope_asked_for_can_only_read() {
        // The consent screen is a promise. Asking for a write scope "just in
        // case" is how a read-only tool quietly stops being one.
        assert!(SCOPE.contains("projects:read"));
        assert!(SCOPE.contains("workspaces:read"));
        assert!(SCOPE.contains("offline"));
        assert!(
            !SCOPE.contains("write"),
            "scope must stay read-only: {SCOPE}"
        );
        assert!(
            !SCOPE.contains("create"),
            "scope must stay read-only: {SCOPE}"
        );
    }

    #[test]
    fn the_client_protocol_is_its_own_constant_not_the_servers() {
        // They are equal today. The point is that they are two decisions:
        // this one is what craft asks Lovable to speak, and `mcp.rs`'s is what
        // craft's own server answers Claude Desktop with. Chasing Lovable must
        // never move ours, so if this assertion is what broke, change the
        // constant above rather than reaching for `mcp::PROTOCOL_VERSION`.
        assert_eq!(PROTOCOL, "2025-11-25");
    }

    #[test]
    fn the_redirect_is_the_loopback_address_lovable_accepts() {
        // Lovable refuses `http://localhost:<port>` with `invalid_request` and
        // accepts `http://127.0.0.1:<port>`. The two are interchangeable
        // almost everywhere else, which is exactly why this is written down.
        let redirect = redirect_uri(53117);
        assert_eq!(redirect, "http://127.0.0.1:53117");
        assert!(!redirect.contains("localhost"));
    }

    // ------------------------------------------------------------ transport ---

    #[test]
    fn a_plain_json_reply_parses() {
        let v = message(
            "application/json",
            r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#,
        )
        .unwrap();
        assert_eq!(v["result"]["ok"], json!(true));
    }

    #[test]
    fn a_reply_framed_as_sse_reads_the_same_as_a_plain_json_one() {
        // The same request may be answered either way, so both have to land on
        // the same value rather than one of them being a protocol error.
        let plain = message(
            "application/json",
            r#"{"jsonrpc":"2.0","id":1,"result":{"n":7}}"#,
        )
        .unwrap();
        let framed = message(
            "text/event-stream; charset=utf-8",
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"n\":7}}\n\n",
        )
        .unwrap();
        assert_eq!(plain, framed);
    }

    #[test]
    fn several_data_lines_in_one_frame_join_into_one_payload() {
        // SSE splits a long payload across `data:` lines; joining them with
        // anything but a newline corrupts JSON that contains strings.
        let v = message(
            "text/event-stream",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\n\
             data: \"result\":{\"n\":7}}\n\n",
        )
        .unwrap();
        assert_eq!(v["result"]["n"], json!(7));
    }

    #[test]
    fn a_keep_alive_comment_is_not_mistaken_for_a_reply() {
        let v = message(
            "text/event-stream",
            ": keep-alive\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":1}\n\n",
        )
        .unwrap();
        assert_eq!(v["result"], json!(1));
    }

    #[test]
    fn a_notification_arriving_first_is_skipped_not_answered() {
        // A server may push progress before the reply. Taking the first frame
        // that parses would hand the caller a notification as its result.
        let v = message(
            "text/event-stream",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\n\
             data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"real\":true}}\n\n",
        )
        .unwrap();
        assert_eq!(v["result"]["real"], json!(true));
    }

    #[test]
    fn a_frame_missing_its_trailing_blank_line_still_flushes() {
        let v = message(
            "text/event-stream",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":2}",
        )
        .unwrap();
        assert_eq!(v["result"], json!(2));
    }

    #[test]
    fn crlf_framing_parses_too() {
        let v = message(
            "text/event-stream",
            "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":3}\r\n\r\n",
        )
        .unwrap();
        assert_eq!(v["result"], json!(3));
    }

    #[test]
    fn an_event_stream_with_no_reply_in_it_is_an_error() {
        let err = message("text/event-stream", ": just a comment\n\n")
            .expect_err("no reply is not a result")
            .to_string();
        assert!(err.contains("no reply"), "unhelpful: {err}");
    }

    #[test]
    fn a_refusal_names_the_command_that_fixes_it() {
        assert!(explain(401, "").contains("craft lovable --link"));
        assert!(explain(403, "").contains("signed in"));
        assert!(explain(429, "").contains("nothing was changed"));
        assert!(explain(503, "").contains("nothing was changed"));
    }

    #[test]
    fn an_error_body_is_clipped_rather_than_pasted_whole() {
        // A failing read_file could otherwise spill a project's source into
        // somebody's terminal.
        let long = "x".repeat(5000);
        assert!(explain(418, &long).len() < 400);
    }

    #[test]
    fn a_listing_that_ended_has_no_next_cursor() {
        assert_eq!(next_cursor(&json!({})), None);
        assert_eq!(next_cursor(&json!({"pagination": {}})), None);
        assert_eq!(
            next_cursor(&json!({"pagination": {"next_cursor": null}})),
            None
        );
        // An empty string is what makes a pager loop forever.
        assert_eq!(
            next_cursor(&json!({"pagination": {"next_cursor": ""}})),
            None
        );
        assert_eq!(
            next_cursor(&json!({"pagination": {"next_cursor": "abc"}})),
            Some("abc")
        );
    }

    #[test]
    fn a_tool_result_prefers_the_structured_field() {
        let v = unwrap_tool(
            "list_projects",
            json!({"content": [{"type": "text", "text": "{\"a\":1}"}],
                   "structuredContent": {"a": 2}}),
        )
        .unwrap();
        assert_eq!(v["a"], json!(2));
    }

    #[test]
    fn a_tool_result_carrying_json_as_text_is_parsed() {
        let v = unwrap_tool(
            "get_project",
            json!({"content": [{"type": "text", "text": "{\"id\":\"p1\"}"}]}),
        )
        .unwrap();
        assert_eq!(v["id"], json!("p1"));
    }

    #[test]
    fn a_tool_that_reported_its_own_failure_is_an_error() {
        let err = unwrap_tool(
            "read_file",
            json!({"isError": true, "content": [{"type": "text", "text": "no such path"}]}),
        )
        .expect_err("isError must not read as a result")
        .to_string();
        assert!(err.contains("no such path"), "unhelpful: {err}");
    }

    /// A server that answers `responses` in order and records what it was
    /// sent, so the transport can be exercised without mcp.lovable.dev.
    /// Shaped after `slack::tests::fake_slack`, with a queue because the
    /// handshake costs a request before the one under test.
    fn fake_mcp(responses: Vec<(u16, &'static str, &'static str)>) -> (String, Requests) {
        use std::io::{Read, Write};
        use std::net::{Ipv4Addr, TcpListener};
        use std::sync::{Arc, Mutex};

        let seen: Requests = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        std::thread::spawn(move || {
            for (status, content_type, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 8192];
                let read = stream.read(&mut buf).unwrap_or(0);
                recorder
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..read]).to_string());

                let reason = if (200..300).contains(&status) {
                    "OK"
                } else {
                    "Error"
                };
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n\
                         Mcp-Session-Id: sess-42\r\nContent-Length: {}\r\n\
                         Connection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });

        (format!("http://127.0.0.1:{port}/"), seen)
    }

    const HELLO: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"lovable"}}}"#;

    #[tokio::test]
    async fn the_handshake_keeps_the_session_and_speaks_the_version_lovable_chose() {
        let (url, seen) = fake_mcp(vec![
            (200, "application/json", HELLO),
            (202, "application/json", ""),
            (
                200,
                "application/json",
                r#"{"jsonrpc":"2.0","id":3,"result":{"structuredContent":{"ok":true}}}"#,
            ),
        ]);

        let mut mcp = Mcp::connect(reqwest::Client::new(), &url, Credential::Bearer("t".into()))
            .await
            .expect("handshake");

        // The server named an older revision than we asked for; later requests
        // must carry its answer, not our ask.
        assert_eq!(mcp.version, "2025-06-18");
        mcp.tool("get_me", json!({})).await.expect("call");

        let requests = seen.lock().unwrap();
        // Header names are case-insensitive on the wire and reqwest sends them
        // lowercased, so the assertion has to be too.
        let last = requests.last().expect("a third request").to_lowercase();
        assert!(
            last.contains("mcp-session-id: sess-42"),
            "no session: {last}"
        );
        assert!(
            last.contains("mcp-protocol-version: 2025-06-18"),
            "wrong version echoed: {last}"
        );
    }

    #[tokio::test]
    async fn an_expired_link_is_explained_as_the_command_that_fixes_it() {
        let (url, _) = fake_mcp(vec![(401, "application/json", r#"{"error":"nope"}"#)]);

        let err = Mcp::connect(reqwest::Client::new(), &url, Credential::Bearer("t".into()))
            .await
            .map(|_| ())
            .expect_err("401 is not a connection")
            .to_string();

        assert!(err.contains("craft lovable --link"), "unhelpful: {err}");
    }

    #[tokio::test]
    async fn a_refusal_is_an_error_even_though_lovable_said_200() {
        // The same trap `slack.rs` records: a rejection wearing a 200.
        let (url, _) = fake_mcp(vec![
            (200, "application/json", HELLO),
            (202, "application/json", ""),
            (
                200,
                "application/json",
                r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"project not found"}}"#,
            ),
        ]);

        let mut mcp = Mcp::connect(reqwest::Client::new(), &url, Credential::Bearer("t".into()))
            .await
            .expect("handshake");

        let err = mcp
            .tool("get_project", json!({}))
            .await
            .expect_err("a rejection must not read as a result")
            .to_string();

        assert!(err.contains("project not found"), "unhelpful: {err}");
    }

    #[tokio::test]
    async fn a_missing_tool_says_the_manual_path_still_works() {
        let (url, _) = fake_mcp(vec![
            (200, "application/json", HELLO),
            (202, "application/json", ""),
            (
                200,
                "text/event-stream",
                "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"error\":{\"code\":-32601,\"message\":\"no\"}}\n\n",
            ),
        ]);

        let mut mcp = Mcp::connect(reqwest::Client::new(), &url, Credential::Bearer("t".into()))
            .await
            .expect("handshake");

        let err = mcp
            .tool("read_file", json!({}))
            .await
            .expect_err("a missing tool is an error")
            .to_string();

        assert!(err.contains("craft use"), "no manual path offered: {err}");
    }

    #[tokio::test]
    async fn a_pager_stops_when_the_cursor_runs_out() {
        let (url, _) = fake_mcp(vec![
            (200, "application/json", HELLO),
            (202, "application/json", ""),
            (
                200,
                "application/json",
                r#"{"jsonrpc":"2.0","id":3,"result":{"structuredContent":{"projects":[{"id":"a"}],"pagination":{"next_cursor":"c2"}}}}"#,
            ),
            (
                200,
                "application/json",
                r#"{"jsonrpc":"2.0","id":4,"result":{"structuredContent":{"projects":[{"id":"b"}]}}}"#,
            ),
        ]);

        let mut mcp = Mcp::connect(reqwest::Client::new(), &url, Credential::Bearer("t".into()))
            .await
            .expect("handshake");

        let page = mcp
            .page_all("list_projects", json!({}), "projects")
            .await
            .expect("two pages");

        assert_eq!(page.items.len(), 2);
        assert!(!page.truncated);
    }

    #[test]
    fn an_api_key_travels_in_lovables_own_header_not_authorization() {
        // The two are not interchangeable: a key sent as a bearer token is
        // simply not authenticated, with a 401 that reads like an expired link.
        let (header, value) = Credential::ApiKey("lov_abc".into()).header();
        assert_eq!(header, "Lovable-API-Key");
        assert_eq!(value, "lov_abc");

        let (header, value) = Credential::Bearer("tok".into()).header();
        assert_eq!(header, "Authorization");
        assert_eq!(value, "Bearer tok");
    }

    // ---------------------------------------------------------------- oauth ---

    #[tokio::test]
    async fn registration_keeps_what_it_needs_to_delete_itself_later() {
        // The real shape, as lovable.dev/oauth/register answers it.
        let (url, _) = fake_mcp(vec![(
            201,
            "application/json",
            r#"{"client_id":"cid1","client_secret_expires_at":0,
                "registration_access_token":"cid1.rat",
                "registration_client_uri":"https://lovable.dev/oauth/register/cid1",
                "redirect_uris":["http://127.0.0.1:53117"],
                "token_endpoint_auth_method":"none"}"#,
        )]);

        let reg = register(&reqwest::Client::new(), &url, "http://127.0.0.1:53117")
            .await
            .expect("registered");

        assert_eq!(reg.client_id, "cid1");
        // Without these, `--unlink` can only forget the client, not remove it.
        assert_eq!(reg.registration_access_token.as_deref(), Some("cid1.rat"));
        assert!(reg
            .registration_client_uri
            .as_deref()
            .is_some_and(|u| u.ends_with("/cid1")));
    }

    #[tokio::test]
    async fn a_refused_registration_offers_the_headless_way_in() {
        let (url, _) = fake_mcp(vec![(
            400,
            "application/json",
            r#"{"error":"invalid_redirect_uri"}"#,
        )]);

        let err = register(&reqwest::Client::new(), &url, "https://example.com")
            .await
            .map(|_| ())
            .expect_err("a refusal is not a registration")
            .to_string();

        assert!(err.contains(API_KEY_ENV), "no fallback offered: {err}");
    }

    #[tokio::test]
    async fn a_registration_lovable_forgot_says_to_link_again() {
        // `invalid_client` at the token endpoint means the client this machine
        // holds is gone from Lovable's side. "Rejected the sign-in" would send
        // somebody hunting for a bad password instead.
        let (url, _) = fake_mcp(vec![(
            401,
            "application/json",
            r#"{"error":"invalid_client","error_description":"unknown client"}"#,
        )]);

        let err = token(
            &reqwest::Client::new(),
            &url,
            &[("grant_type", "refresh_token")],
        )
        .await
        .map(|_| ())
        .expect_err("an unknown client is not a grant")
        .to_string();

        assert!(err.contains("--link"), "unhelpful: {err}");
        assert!(err.contains("registration"), "unhelpful: {err}");
    }

    #[tokio::test]
    async fn a_grant_carries_the_refresh_token_offline_was_asked_for() {
        let (url, _) = fake_mcp(vec![(
            200,
            "application/json",
            r#"{"access_token":"at1","refresh_token":"rt1","expires_in":3600,
                "token_type":"Bearer","scope":"offline projects:read workspaces:read"}"#,
        )]);

        let body = token(
            &reqwest::Client::new(),
            &url,
            &[("grant_type", "authorization_code")],
        )
        .await
        .expect("granted");

        assert_eq!(body.access_token, "at1");
        assert_eq!(body.refresh_token.as_deref(), Some("rt1"));
        assert_eq!(body.expires_in, 3600);
    }

    #[test]
    fn a_refresh_that_rotated_nothing_still_parses() {
        // An authorization server may answer a refresh without a new refresh
        // token, meaning "keep the one you have". Treating the absence as an
        // error would cost a re-link on every token expiry.
        let body: TokenResponse =
            serde_json::from_str(r#"{"access_token":"at2","expires_in":3600}"#).unwrap();
        assert!(body.refresh_token.is_none());
        assert_eq!(body.access_token, "at2");
    }

    #[test]
    fn a_credential_does_not_print_itself() {
        // The reason the token lives at 0600 outside config.toml is undone by
        // a Debug that pastes it into an error message or a log line.
        let shown = format!("{:?}", Credential::Bearer("super-secret".into()));
        assert!(!shown.contains("super-secret"), "leaked: {shown}");
        let shown = format!("{:?}", Credential::ApiKey("lov_secret".into()));
        assert!(!shown.contains("lov_secret"), "leaked: {shown}");
    }

    // ------------------------------------------------------------ discovery ---

    #[test]
    fn the_tag_craft_itself_prints_reads_back_as_one_id() {
        // The snippet carries the id twice — once in the script src, once in
        // the `gtag('config', …)` call — and `configure`'s own test pins that.
        // Reading it as two properties would be a false double-counting
        // warning on every correctly tagged site, so this ties the two modules
        // together: if that snippet changes shape, this fails.
        let snippet = crate::configure::tag_snippet("G-1A2BCD345E");
        assert_eq!(measurement_ids(&snippet), vec!["G-1A2BCD345E".to_string()]);
    }

    #[test]
    fn the_placeholder_in_the_setup_guide_is_not_a_measurement_id() {
        // docs/setup-ga4.html prints this, so it is very likely pasted into
        // somebody's project inside a comment.
        assert!(measurement_ids("gtag('config', 'G-XXXXXXXXXX');").is_empty());
    }

    #[test]
    fn nothing_that_merely_looks_like_one_is_taken_for_one() {
        // A Tag Manager container, Universal Analytics, a lowercase tail, and
        // a `G-` that is the end of another word.
        for text in [
            "GTM-ABC1234",
            "UA-12345-1",
            "g-1a2bcd345e",
            "SOMETAG-1A2BCD345E",
            "G-SHORT",
            "G-",
            "trailing G-",
        ] {
            assert!(
                measurement_ids(text).is_empty(),
                "{text} was read as a measurement id"
            );
        }
    }

    #[test]
    fn two_different_tags_in_one_file_are_both_reported() {
        let both = measurement_ids("gtag('config','G-1A2BCD345E'); gtag('config','G-9Z8YXW765V');");
        assert_eq!(both, vec!["G-1A2BCD345E", "G-9Z8YXW765V"]);
    }

    #[test]
    fn the_scan_reads_index_html_first_and_never_opens_node_modules() {
        let paths: Vec<String> = [
            "node_modules/react/index.html",
            "src/App.tsx",
            "README.md",
            "index.html",
            "package-lock.json",
            "src/main.tsx",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let ranked = rank(&paths);
        assert_eq!(ranked.first().map(String::as_str), Some("index.html"));
        assert!(!ranked.iter().any(|p| p.contains("node_modules")));
        assert!(!ranked.iter().any(|p| p.ends_with(".md")));
        assert!(!ranked.iter().any(|p| p.contains("package-lock")));
    }

    #[test]
    fn a_projects_secrets_are_never_opened_but_the_example_is() {
        // `projects:read` is enough to fetch `.env`. That is exactly why this
        // has to refuse to.
        assert!(score(".env").is_none());
        assert!(score(".env.local").is_none());
        assert!(score(".env.production").is_none());
        assert!(score(".env.example").is_some());
    }

    #[test]
    fn the_scan_never_opens_more_than_the_cap() {
        let paths: Vec<String> = (0..50).map(|i| format!("src/analytics{i}.ts")).collect();
        assert_eq!(rank(&paths).len(), READ_CAP);
    }

    #[test]
    fn a_lovable_env_var_install_is_recognised_rather_than_called_empty() {
        // What Lovable's own Google Analytics connector leaves in the source:
        // the plumbing, and no id anywhere.
        let src = "const id = import.meta.env.VITE_GA_MEASUREMENT_ID;\n\
                   ReactGA.initialize(id);";
        assert!(measurement_ids(src).is_empty());
        assert_eq!(
            wiring(src).as_deref(),
            Some("import.meta.env.VITE_GA_MEASUREMENT_ID")
        );
    }

    #[test]
    fn a_placeholder_key_in_the_example_env_names_where_the_id_lives() {
        assert_eq!(
            wiring("VITE_GA_MEASUREMENT_ID=\n").as_deref(),
            Some("VITE_GA_MEASUREMENT_ID")
        );
    }

    #[test]
    fn plain_gtag_plumbing_counts_as_wiring_too() {
        assert!(wiring("<script src=\"https://www.googletagmanager.com/gtag/js?id=\">").is_some());
        assert!(wiring("import ReactGA from 'react-ga4';").is_some());
        assert!(wiring("<h1>hello</h1>").is_none());
    }

    #[test]
    fn one_id_in_two_files_is_still_one_property_and_worth_saying() {
        let scan = Scan {
            hits: vec![
                Hit {
                    path: "index.html".into(),
                    id: "G-1A2BCD345E".into(),
                },
                Hit {
                    path: "src/main.tsx".into(),
                    id: "G-1A2BCD345E".into(),
                },
            ],
            ..Default::default()
        };
        match decide(&scan) {
            Found::One { id, paths } => {
                assert_eq!(id, "G-1A2BCD345E");
                // Two tags for one property still doubles every number.
                assert_eq!(paths.len(), 2);
            }
            other => panic!("expected one id, got {other:?}"),
        }
    }

    #[test]
    fn two_ids_are_never_guessed_between() {
        let scan = Scan {
            hits: vec![
                Hit {
                    path: "index.html".into(),
                    id: "G-1A2BCD345E".into(),
                },
                Hit {
                    path: "src/analytics.ts".into(),
                    id: "G-9Z8YXW765V".into(),
                },
            ],
            ..Default::default()
        };
        assert!(matches!(decide(&scan), Found::Many(hits) if hits.len() == 2));
    }

    #[test]
    fn wiring_without_an_id_decides_wired_not_none() {
        let scan = Scan {
            wired: vec![Wire {
                path: "src/main.tsx".into(),
                token: "import.meta.env.VITE_GA_MEASUREMENT_ID".into(),
            }],
            ..Default::default()
        };
        assert!(matches!(decide(&scan), Found::Wired(w) if w.len() == 1));
    }

    #[test]
    fn a_project_with_no_analytics_at_all_decides_none() {
        assert_eq!(decide(&Scan::default()), Found::None);
    }

    #[test]
    fn the_only_project_is_taken_without_being_asked_about() {
        let one = vec![Project {
            id: "p1".into(),
            name: Some("solo".into()),
            url: None,
            ..Default::default()
        }];
        assert_eq!(pick(&one, None).unwrap().id, "p1");
    }

    #[test]
    fn several_projects_print_the_command_that_picks_one() {
        let many = vec![
            Project {
                id: "p1".into(),
                name: Some("a".into()),
                url: None,
                ..Default::default()
            },
            Project {
                id: "p2".into(),
                name: Some("b".into()),
                url: None,
                ..Default::default()
            },
        ];
        let err = pick(&many, None)
            .map(|_| ())
            .expect_err("ambiguous")
            .to_string();
        assert!(err.contains("--project"), "no way forward offered: {err}");

        // By id, and by name, case-insensitively.
        assert_eq!(pick(&many, Some("p2")).unwrap().id, "p2");
        assert_eq!(pick(&many, Some("A")).unwrap().id, "p1");
    }

    #[test]
    fn an_account_with_no_projects_says_so_rather_than_failing_oddly() {
        let err = pick(&[], None).map(|_| ()).expect_err("none").to_string();
        assert!(err.contains("no projects"), "unhelpful: {err}");
    }

    #[test]
    fn a_project_is_read_out_of_whichever_field_names_it() {
        // The tool surface is young; a renamed field should cost the domain
        // route, not the command.
        let p = Project::from_value(&json!({"id": "p1", "name": "app"})).unwrap();
        assert_eq!(p.url, None);

        let p = Project::from_value(
            &json!({"project_id": "p2", "title": "app2", "published_url": "https://x.lovable.app"}),
        )
        .unwrap();
        assert_eq!(p.id, "p2");
        assert_eq!(p.url.as_deref(), Some("https://x.lovable.app"));

        // Without an id there is nothing to call a project.
        assert!(Project::from_value(&json!({"name": "nameless"})).is_none());
    }

    // ------------------------------------------------------- reconciliation ---

    #[test]
    fn the_tag_wins_because_it_is_what_the_site_actually_sends_to() {
        // A stream's default_uri is free text somebody typed into the console.
        assert_eq!(
            precedence(Some("111"), None, true),
            Winner::Tag { disagrees: false }
        );
        assert_eq!(
            precedence(Some("111"), Some("111"), true),
            Winner::Tag { disagrees: false }
        );
    }

    #[test]
    fn two_properties_claiming_one_site_is_reported_not_hidden() {
        // Split numbers is the condition `craft configure` exists to prevent,
        // so the disagreement has to reach the user even though the tag wins.
        assert_eq!(
            precedence(Some("111"), Some("222"), true),
            Winner::Tag { disagrees: true }
        );
    }

    #[test]
    fn a_tag_this_account_cannot_see_is_a_finding_not_a_fallback() {
        // The site tags G-A…, and this Google account can only see a property
        // claiming the domain. Setting that one would make every later number
        // come from a property the site never sends to.
        assert_eq!(precedence(None, Some("222"), true), Winner::Stalemate);
    }

    #[test]
    fn an_env_var_install_falls_through_to_the_domain() {
        // No literal id in the source is not the same as no analytics, so the
        // domain route is allowed to decide here.
        assert_eq!(precedence(None, Some("222"), false), Winner::Domain);
    }

    #[test]
    fn nothing_anywhere_is_the_mint_branch() {
        assert_eq!(precedence(None, None, false), Winner::Nothing);
        assert_eq!(precedence(None, None, true), Winner::TagUnseen);
    }

    #[test]
    fn a_listing_is_found_even_when_the_array_is_named_something_else() {
        // A renamed field must not read as "you have nothing" — which is the
        // shape of failure this surface actually produces.
        let named = json!({"projects": [{"id": "p1"}], "pagination": {}});
        assert_eq!(rows(&named, "projects").map(Vec::len), Some(1));

        // The key we guessed is wrong, but there is only one array in the page.
        let renamed = json!({"items": [{"id": "p1"}], "pagination": {}});
        assert_eq!(rows(&renamed, "projects").map(Vec::len), Some(1));

        // A bare array is a listing too.
        let bare = json!([{"id": "p1"}, {"id": "p2"}]);
        assert_eq!(rows(&bare, "projects").map(Vec::len), Some(2));

        assert!(rows(&json!({"pagination": {}}), "projects").is_none());
    }

    #[tokio::test]
    async fn projects_are_listed_per_workspace_because_lovable_requires_one() {
        // The regression this exists for: `list_projects` takes a required
        // `workspace_id`, and calling it without one comes back as
        // `-32602 Invalid arguments`, not as an empty list.
        let (url, seen) = fake_mcp(vec![
            (200, "application/json", HELLO),
            (202, "application/json", ""),
            (
                200,
                "application/json",
                r#"{"jsonrpc":"2.0","id":3,"result":{"structuredContent":{"workspaces":[{"id":"w1","name":"Acme"}]}}}"#,
            ),
            (
                200,
                "application/json",
                r#"{"jsonrpc":"2.0","id":4,"result":{"structuredContent":{"projects":[{"id":"p1","name":"app"}]}}}"#,
            ),
        ]);

        let mut mcp = Mcp::connect(reqwest::Client::new(), &url, Credential::Bearer("t".into()))
            .await
            .expect("handshake");

        let (projects, truncated) = projects_in_account(&mut mcp).await.expect("listed");

        assert!(!truncated);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, "p1");
        // The workspace travels with the project, so `--project` can stay a
        // single value and the record knows where the project lives.
        assert_eq!(projects[0].workspace.as_deref(), Some("w1"));
        assert_eq!(projects[0].display(), "Acme / app (p1)");

        let requests = seen.lock().unwrap();
        let listing = requests.last().expect("a list_projects request");
        assert!(
            listing.contains(r#""workspace_id":"w1""#),
            "list_projects went out without the workspace it requires: {listing}"
        );
    }

    /// The real listing of a Lovable project, minus most of the generated
    /// component library. This template has **no `index.html` and no
    /// `src/main.tsx`** — it routes from `src/routes/` — which is the shape
    /// that made the first version of the scan read nothing at all.
    const REAL_PROJECT: &[&str] = &[
        ".gitignore",
        ".lovable/project.json",
        ".prettierrc",
        "AGENTS.md",
        "README.md",
        "bun.lock",
        "components.json",
        "eslint.config.js",
        "package.json",
        "public/favicon.ico",
        "public/robots.txt",
        "roadmap.md",
        "src/assets/tailslide-photo.png.asset.json",
        "src/components/ui/accordion.tsx",
        "src/components/ui/button.tsx",
        "src/components/ui/card.tsx",
        "src/hooks/use-mobile.tsx",
        "src/lib/error-capture.ts",
        "src/lib/utils.ts",
        "src/routeTree.gen.ts",
        "src/router.tsx",
        "src/routes/README.md",
        "src/routes/__root.tsx",
        "src/routes/index.tsx",
        "src/server.ts",
        "src/start.ts",
        "src/styles.css",
        "tsconfig.json",
        "vite.config.ts",
    ];

    #[test]
    fn a_project_with_no_index_html_is_still_read() {
        // The regression: ranking against the documented Vite layout and
        // refusing everything else read 0 of 76 files and reported "no GA4
        // tag", which is indistinguishable from having looked.
        let paths: Vec<String> = REAL_PROJECT.iter().map(|s| s.to_string()).collect();
        let ranked = rank(&paths);

        assert!(
            !ranked.is_empty(),
            "a real Lovable project must not rank as nothing to read"
        );

        // The document root is where a tag goes in this template, so it has to
        // be in the budget rather than merely eligible.
        assert!(
            ranked.iter().any(|p| p == "src/routes/__root.tsx"),
            "the root route was not read: {ranked:?}"
        );
        assert!(ranked.iter().any(|p| p == "src/routes/index.tsx"));
    }

    #[test]
    fn the_generated_component_library_never_crowds_out_the_entry_points() {
        // shadcn ships dozens of these. Reading them costs a round trip each
        // and none of them has ever carried a measurement id.
        let paths: Vec<String> = REAL_PROJECT.iter().map(|s| s.to_string()).collect();
        let ranked = rank(&paths);
        assert!(
            !ranked.iter().any(|p| p.starts_with("src/components/ui/")),
            "generated components were opened: {ranked:?}"
        );
        // Nor the lockfiles, prose, or assets.
        assert!(!ranked.iter().any(|p| p.ends_with(".md")));
        assert!(!ranked.iter().any(|p| p.ends_with(".lock")));
        assert!(!ranked.iter().any(|p| p.ends_with(".css")));
    }

    #[test]
    fn an_analytics_helper_is_read_even_parked_among_the_components() {
        // The noise rule must not outrank a filename that says what it is.
        let paths = vec!["src/components/ui/analytics.tsx".to_string()];
        assert_eq!(rank(&paths), vec!["src/components/ui/analytics.tsx"]);
    }

    #[test]
    fn the_documented_layout_still_ranks_first_where_it_exists() {
        // Broadening the scan must not cost the common case its priority.
        let paths: Vec<String> = [
            "src/routes/__root.tsx",
            "vite.config.ts",
            "index.html",
            "src/main.tsx",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(rank(&paths).first().map(String::as_str), Some("index.html"));
    }
}
