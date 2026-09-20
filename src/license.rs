//! The subscription record.
//!
//! Stripe cannot reach a binary on somebody's laptop, and a Stripe key shipped
//! inside a downloadable binary is a key that has leaked. So the payment and
//! the machine meet in the middle, at a Supabase project:
//!
//! 1. `craft subscribe` mints a token, writes a pending row keyed to the signed
//!    in Google account, and opens the site's pricing page carrying that token,
//!    which the page forwards onto Stripe as the checkout's
//!    `client_reference_id`. The page rather than the card field, because a
//!    command should not spend somebody's money one click sooner than they
//!    meant to.
//! 2. Stripe's webhook (`supabase/functions/stripe-webhook`) fills that row in
//!    with the customer, the subscription and its status, and keeps it current
//!    as the subscription renews, lapses or is cancelled.
//! 3. Every `craft dash` and `craft mcp` asks Supabase what the account's
//!    status is and caches the answer.
//!
//! Keying on the Google account id — not on the token, not on the machine — is
//! what makes a new laptop work: sign in, and the subscription is already
//! there. The token is kept as a second key so a checkout that finished before
//! the identity landed still resolves.
//!
//! Only two calls exist, both `security definer` RPCs, and the binary carries
//! the anon key, which is public by design: the table itself is closed to it.
//! The worst an anon key can do here is ask about one account and write one
//! pending row.
//!
//! Nothing here is a hard gate. The cached answer lands in `supporter`, a line
//! of TOML in a config anybody can edit, in a binary anybody can rebuild. It
//! closes the loop for people who paid; it does not stand between anybody and
//! their own analytics.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

use crate::auth::Account;
use crate::config::Config;

/// One of the three Anacraft plans, in the order somebody climbs them.
///
/// Basic — $2.99 — is `craft configure` and `craft watch`. Pro — $5.99 — adds
/// `craft audit` and Slack alerts. Elite — $9.99 — adds `craft mcp` and the
/// rest of the AI-tool integration. A plan contains everything below it, so every gate is one
/// `meets` comparison and every dollar figure lives here, once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Tier {
    Basic,
    Pro,
    Elite,
}

impl Tier {
    /// 1, 2, 3 — the order the plans climb in, which is what `meets` compares.
    pub fn level(self) -> u8 {
        match self {
            Tier::Basic => 1,
            Tier::Pro => 2,
            Tier::Elite => 3,
        }
    }

    /// The word the config and the service use, lowercase: `tier = "elite"`.
    pub fn name(self) -> &'static str {
        match self {
            Tier::Basic => "basic",
            Tier::Pro => "pro",
            Tier::Elite => "elite",
        }
    }

    pub fn parse(name: &str) -> Option<Tier> {
        match name {
            "basic" => Some(Tier::Basic),
            "pro" => Some(Tier::Pro),
            "elite" => Some(Tier::Elite),
            _ => None,
        }
    }

    /// The name the site and the dashboard wear. Basic keeps the plain word —
    /// "Anacrafter" is the plan that has been sold since the start — and the
    /// two above it tag theirs on.
    pub fn label(self) -> &'static str {
        match self {
            Tier::Basic => "Anacrafter",
            Tier::Pro => "Anacrafter Pro",
            Tier::Elite => "Anacrafter Elite",
        }
    }

    /// The monthly line, quoted in the ask and by `craft subscribe`.
    pub fn monthly(self) -> &'static str {
        match self {
            Tier::Basic => "$2.99/month",
            Tier::Pro => "$5.99/month",
            Tier::Elite => "$9.99/month",
        }
    }

    /// Whether this plan includes `required` — true when they are the same or
    /// this is the one above it.
    pub fn meets(self, required: Tier) -> bool {
        self.level() >= required.level()
    }
}

impl Serialize for Tier {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for Tier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let name = String::deserialize(deserializer)?;
        Tier::parse(&name).ok_or_else(|| serde::de::Error::custom(format!("unknown plan {name:?}")))
    }
}

/// Baked in at build time by the release pipeline, the way the OAuth client is:
///   ANACRAFT_SUPABASE_URL=... ANACRAFT_SUPABASE_KEY=... cargo build --release
///
/// Not written into the source. The publishable key is public in the sense that
/// it travels in every request the binary makes and the table is closed to it —
/// RLS with no policies, two `security definer` functions that answer about a
/// single account — but "safe to send" is not "worth committing": a key in git
/// is a key that outlives its rotation. See `supabase/README.md`.
///
/// Both are overridable at runtime, which is how a debug build points at a
/// local `supabase start`, and a build with neither simply has no subscription
/// lookup.
const BUILTIN_URL: Option<&str> = option_env!("ANACRAFT_SUPABASE_URL");
const BUILTIN_KEY: Option<&str> = option_env!("ANACRAFT_SUPABASE_KEY");

/// How long a confirmed answer is trusted before asking again. A subscription
/// changes state at most monthly; a dashboard opened twenty times a day should
/// not ask twenty times.
const TTL: Duration = Duration::hours(12);

/// How long a subscriber stays a subscriber while the check cannot be reached.
/// A plane, a hotel wifi, or Supabase having a bad afternoon is not a reason to
/// take the star off somebody who paid.
const GRACE: Duration = Duration::days(14);

/// Subscription calls get their own short timeout: this runs on the way into
/// the dashboard, and a hanging socket must never be the thing standing between
/// somebody and their numbers.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The project this build talks to, if it has one. `None` — an empty URL or key
/// — means no subscription lookup at all, which is a working build: it just
/// falls back to the flag in the config.
pub fn project() -> Option<(String, String)> {
    let url = env_or("ANACRAFT_SUPABASE_URL", BUILTIN_URL)?;
    let key = env_or("ANACRAFT_SUPABASE_KEY", BUILTIN_KEY)?;
    Some((url.trim_end_matches('/').to_string(), key))
}

fn env_or(var: &str, builtin: Option<&str>) -> Option<String> {
    match std::env::var(var) {
        Ok(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
        _ => builtin
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string),
    }
}

/// What Supabase knows about an account's subscription.
///
/// `status` is Stripe's own status string wherever there is one, so a value
/// this build has never heard of ("paused", say) still round-trips and gets
/// reported rather than swallowed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Status {
    #[serde(default)]
    pub status: String,
    /// When the first payment landed, for the "subscriber since" line.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    /// The account's own flag, kept by Supabase across every payment attached
    /// to it. This is what `supporter` mirrors; `status` is what gets said out
    /// loud. Absent from an older service, in which case the status decides.
    #[serde(default)]
    pub subscribed: Option<bool>,
    /// The Anacrafter's number, in the order the first payment landed.
    ///
    /// Assigned once and kept. Somebody who paid early stays early whether or
    /// not the subscription is live today — it is a founder's number, not a
    /// seat. Absent from an older service and from an account that has never
    /// paid, which is why it is an `Option` and not a zero.
    #[serde(default)]
    pub founder: Option<u32>,
    /// The plan this subscription is on — "basic", "pro" or "elite", as the
    /// service spells it. Absent from a row that predates tiers, in which case
    /// [`Status::tier`] reads the subscription as Basic: that is what the old
    /// $2.99 plan was.
    #[serde(default)]
    pub tier: Option<String>,
}

impl Status {
    /// The account flag wins where there is one: somebody with two payments on
    /// one account is a subscriber if either of them is live, and the row this
    /// lookup happened to return is not the place to work that out.
    ///
    /// Trialing counts either way — Stripe is charging them on schedule.
    pub fn is_active(&self) -> bool {
        match self.subscribed {
            Some(subscribed) => subscribed,
            None => matches!(self.status.as_str(), "active" | "trialing"),
        }
    }

    /// Whether no payment has ever landed on this row — so it says nothing
    /// about whether the account is subscribed.
    ///
    /// Two shapes of that. `pending` is where every token sits between opening
    /// the browser and the payment clearing. `expired` is the checkout Stripe
    /// gave up on about a day later, written by the webhook so a row stops
    /// claiming to be a payment in flight forever.
    ///
    /// Both are the *absence* of evidence, which is why neither clears the
    /// `supporter` flag: somebody who subscribed before any of this existed has
    /// a hand-set flag and no live row, and an abandoned checkout must not be
    /// read as the cancellation they never made. A cancellation says
    /// `canceled`, and that is an answer.
    ///
    /// An account flag that is already up is an answer too, not a wait.
    pub fn is_pending(&self) -> bool {
        !self.is_active()
            && (self.status.is_empty() || self.status == "pending" || self.status == "expired")
    }

    pub fn label(&self) -> &str {
        if self.status.is_empty() {
            "pending"
        } else {
            &self.status
        }
    }

    /// The plan a live subscription is on.
    ///
    /// A subscription that is not live has no plan at all — a cancelled Elite
    /// subscription is not Pro. And a live one with no tier to read is Basic:
    /// the row predates tiers, and Basic is what the $2.99 subscription always
    /// was, so nobody is ever downgraded by an older record.
    pub fn tier(&self) -> Option<Tier> {
        if !self.is_active() {
            return None;
        }
        Some(
            self.tier
                .as_deref()
                .and_then(Tier::parse)
                .unwrap_or(Tier::Basic),
        )
    }
}

/// The local half of the record: which checkout this machine started, whose
/// account it was for, and the last answer Supabase gave.
///
/// Written 0600 beside the OAuth tokens rather than into the shareable config —
/// it names an account, and people push their dotfiles to public repos.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Record {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Google account id the last check was made for. A different account
    /// signing in on this machine invalidates the cache rather than inheriting
    /// somebody else's answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default)]
    pub status: Status,
    /// When Supabase last actually answered. `None` means it never has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<DateTime<Utc>>,
}

impl Record {
    fn path() -> Result<PathBuf> {
        Ok(crate::config::home()?.join("license.json"))
    }

    pub fn load() -> Record {
        // Every failure here — no file, no home directory, hand-mangled JSON —
        // means the same thing: nothing is known yet. None of them is worth
        // failing a command over.
        Self::path()
            .ok()
            .filter(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(self)?;
        crate::config::write_private(&Self::path()?, &raw)
    }

    /// Remember an answer that just came back, so the next launch does not
    /// have to ask. Keeps whichever key it already had if the caller has none.
    pub fn confirm(&self, account: Option<&Account>, status: &Status) -> Result<()> {
        Record {
            token: self.token.clone(),
            user_id: account
                .map(|a| a.sub.clone())
                .or_else(|| self.user_id.clone()),
            status: status.clone(),
            checked: Some(Utc::now()),
        }
        .save()
    }

    /// Whether the cached answer still speaks for `account`, and is young
    /// enough to trust without asking again.
    ///
    /// Only a *paid* answer is ever cached this way. "Not subscribed" is the
    /// state somebody is actively trying to leave — they have just paid, or
    /// just signed in on a new laptop — so it is re-asked every launch. That
    /// costs one request for someone who is not subscribed, and saves the
    /// twelve hours they would otherwise spend wondering why nothing happened.
    fn is_fresh(&self, account: Option<&Account>, now: DateTime<Utc>) -> bool {
        if !self.status.is_active() {
            return false;
        }
        let Some(checked) = self.checked else {
            return false;
        };
        if let Some(account) = account {
            // A cache belonging to another Google account says nothing about
            // this one.
            if self.user_id.as_deref() != Some(account.sub.as_str()) {
                return false;
            }
        }
        now - checked < TTL
    }

    /// Whether the local evidence — the saved token, and the `supporter` flag
    /// beside it — is this account's to use.
    ///
    /// Both are per-machine and a subscription is per-account, which is the
    /// whole of what this guards. A record naming somebody else says nothing
    /// about the login in front of it: their flag must not vouch for this
    /// account, their token must not be offered to the lookup on its behalf,
    /// and their row must not be claimed for it. Signing back in as the
    /// account that owns them puts all three back.
    ///
    /// No account signed in is not a mismatch — there is no identity to
    /// contradict, and a flag set by hand on a build with no lookup behind it
    /// is the case `verdict` exists to protect. Neither is a record that names
    /// nobody: that is the anonymous checkout, waiting to be adopted by the
    /// first account that asks.
    pub fn speaks_for(&self, account: Option<&Account>) -> bool {
        match (self.user_id.as_deref(), account) {
            (Some(owner), Some(account)) => owner == account.sub,
            _ => true,
        }
    }

    /// Whether an unreachable check should keep the subscription up.
    fn within_grace(&self, now: DateTime<Utc>) -> bool {
        self.status.is_active() && matches!(self.checked, Some(at) if now - at < GRACE)
    }
}

/// Drop everything this machine remembers about the subscription.
///
/// `craft logout` means forget me, and this is the half of that which is not
/// the OAuth token: a record naming an account, and a flag in the config
/// standing for it. Neither can be left behind for the next person to sign in
/// on this machine — which is a laptop handed over, or a shared box, and the
/// case `Record::speaks_for` and the migration behind it are the deeper
/// answers to.
///
/// Nothing is lost by it. The record is a cache of an answer the service still
/// has, and the next `craft login` asks again and writes both back.
pub fn forget() -> Result<()> {
    let path = Record::path()?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    set_plan(None)?;
    Ok(())
}

/// A random token, handed to Stripe as the checkout's `client_reference_id` so
/// the webhook can find the row this machine started.
pub fn mint_token() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(40)
        .map(char::from)
        .collect()
}

/// Where to send somebody who wants to subscribe, carrying the token and — so
/// the Stripe customer matches the Google account rather than whatever they
/// type — the signed-in email.
///
/// `link` is the site's pricing page rather than the Payment Link behind it
/// (see [`crate::PRICING_URL`]), and both parameters are named the way Stripe
/// names them because the page forwards them straight onto its Subscribe
/// button. That keeps one shape of URL for whichever hop it lands on first.
pub fn checkout_url(link: &str, token: &str, email: Option<&str>) -> String {
    let mut url = format!(
        "{link}{}client_reference_id={token}",
        if link.contains('?') { '&' } else { '?' }
    );
    if let Some(email) = email.filter(|e| !e.is_empty()) {
        url.push_str(&format!("&prefilled_email={}", encode(email)));
    }
    url
}

/// Whether this account has already paid — asked over the wire, in the last
/// moment before anybody is sent to a checkout.
///
/// Not [`sync`], and the difference is the whole point of it. `sync` answers a
/// paid-up account from a cache up to `TTL` old, and registers the account only
/// when the local record has not seen it. The payment this is looking for is
/// the one a cache cannot know about: made in a browser minutes ago, on the
/// pricing page, under the same email, possibly on another machine. So this
/// registers first — `link_account` is what adopts a payment that arrived with
/// nobody attached to it — and then asks.
///
/// A `Some` is remembered on the way past, both in the record and in
/// `supporter`, so the rest of the run behaves like the subscriber it just
/// found. Quiet on every failure: nothing here is a gate, and the worst an
/// unreachable service can do is leave somebody looking at a page they can
/// close.
pub async fn already_paid(account: Option<&Account>, token: Option<&str>) -> Option<Status> {
    project()?;
    if account.is_none() && token.is_none() {
        return None;
    }
    if let Some(account) = account {
        let _ = link(account).await;
    }
    let status = fetch(account, token).await.ok()?;
    let _ = Record::load().confirm(account, &status);
    if !status.is_active() {
        return None;
    }
    let _ = set_plan(status.tier());
    Some(status)
}

/// Percent-encode an email for a query string. Only `@` and `+` really matter,
/// but the unreserved set is the honest rule.
pub(crate) fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("building the subscription HTTP client")
}

/// `pub(crate)` for `craft burn`, which talks to the same project through the
/// same anon key and has no reason to build a second client to do it.
pub(crate) async fn rpc(name: &str, body: serde_json::Value) -> Result<String> {
    let (url, key) = project().context("this build has no subscription service configured")?;
    let res = client()?
        .post(format!("{url}/rest/v1/rpc/{name}"))
        .header("apikey", &key)
        .header("Authorization", format!("Bearer {key}"))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("contacting {url}"))?;

    let code = res.status();
    let text = res.text().await.unwrap_or_default();
    if !code.is_success() {
        anyhow::bail!("subscription service returned {code}: {text}");
    }
    Ok(text)
}

/// Write the pending row that ties this checkout to the signed-in account,
/// before the browser ever opens. The webhook fills in the rest.
pub async fn claim(token: &str, account: &Account) -> Result<()> {
    rpc(
        "claim_checkout",
        json!({
            "p_token": token,
            "p_user_id": account.sub,
            "p_email": account.email,
        }),
    )
    .await
    .map(|_| ())
}

/// Register the signed-in account, and let it adopt a payment that arrived
/// with nobody attached — a checkout made from the website, or from a build
/// that never asked who was signed in.
///
/// Called on `craft login` and whenever a machine notices it has an account the
/// local record does not know about. Idempotent, and best-effort at every call
/// site: failing to register is not a reason to fail a login.
pub async fn link(account: &Account) -> Result<()> {
    rpc(
        "link_account",
        json!({ "p_user_id": account.sub, "p_email": account.email }),
    )
    .await
    .map(|_| ())
}

/// Ask Supabase where an account stands — by account id, by the email on it,
/// or, failing both, by a token this machine minted.
///
/// The email is the third key and the newest, and it is here because the first
/// one is not as stable as it looks. `user_id` is Google's `sub`, which is
/// per-account and not per-person: pay from the browser and sign in with the
/// same address under a different Google account, or have a workspace account
/// re-created, and the payment is attached to an id nothing will ever ask
/// about again. The address is what the human has, and what they typed into
/// Stripe. The service decides what an email may answer for — see the
/// `email_answers_too` migration — and this only stops leaving it out.
///
/// A service that predates that migration has a two-argument function and
/// PostgREST will not match a call carrying a third, so a failure is retried
/// without it. That keeps a new binary working against an old project instead
/// of reading "could not reach the check" and sending a subscriber to pay.
pub async fn fetch(account: Option<&Account>, token: Option<&str>) -> Result<Status> {
    let email = account
        .and_then(|a| a.email.as_deref())
        .filter(|e| !e.is_empty());
    let by_account_and_email = json!({
        "p_user_id": account.map(|a| a.sub.as_str()),
        "p_token": token,
        "p_email": email,
    });

    if email.is_some() {
        match rpc("subscription_status", by_account_and_email).await {
            Ok(body) => return parse(&body),
            Err(err) => {
                let by_account = json!({
                    "p_user_id": account.map(|a| a.sub.as_str()),
                    "p_token": token,
                });
                return match rpc("subscription_status", by_account).await {
                    Ok(body) => parse(&body),
                    // The first error is the one worth reporting: the second
                    // call is a guess about an older service, and its failure
                    // says nothing the first has not already said.
                    Err(_) => Err(err),
                };
            }
        }
    }

    parse(&rpc("subscription_status", by_account_and_email).await?)
}

/// Split out from `fetch` so every shape PostgREST can answer with is testable
/// without a network.
fn parse(body: &str) -> Result<Status> {
    let value: serde_json::Value = serde_json::from_str(body)
        .with_context(|| format!("parsing subscription status: {body}"))?;
    // A `returns table(...)` RPC answers with an array; a scalar one with an
    // object. Accept both, and read no rows as "nothing recorded yet".
    let row = match value {
        serde_json::Value::Array(rows) => match rows.into_iter().next() {
            Some(row) => row,
            None => return Ok(Status::default()),
        },
        serde_json::Value::Null => return Ok(Status::default()),
        other => other,
    };
    serde_json::from_value(row).context("reading the subscription row")
}

/// The launch-time check, run on the way into the dashboard, `craft watch`
/// and the MCP server.
///
/// Cheap by design: a fresh cache answers without a request, an unreachable
/// service keeps a paid-up subscriber going for `GRACE`, and a build with no
/// project configured leaves the hand-set flag exactly as it found it. Returns
/// the plan this machine should behave as — `None` is "not subscribed", and is
/// also what a cancelled subscription becomes.
pub async fn sync(cfg: &Config) -> Option<Tier> {
    let Some(_) = project() else {
        return cfg.tier();
    };
    let account = crate::auth::Auth::account().ok().flatten();
    let record = Record::load();
    let now = Utc::now();

    // Whose evidence this machine is holding. Everything below that could
    // answer "subscribed" without a lookup saying so — the flag, the grace
    // window, the saved token — is gated on this; see `Record::speaks_for`.
    let mine = record.speaks_for(account.as_ref());

    // A flag left behind by another account comes down on the way past. It is
    // the one piece of local state that outlives a sign-out, and every gate in
    // the binary reads it, so leaving it standing while answering `None` would
    // hand the next command the answer this one just refused.
    if !mine {
        if cfg.supporter {
            let _ = set_plan(None);
        }
        return None;
    }

    // A token belongs to the checkout that minted it. Offering somebody
    // else's to the lookup asks it about their subscription and writes the
    // answer down as this account's.
    let token = if mine { record.token.clone() } else { None };

    if record.is_fresh(account.as_ref(), now) {
        let tier = record.status.tier();
        // The cache is the answer, so the config has to agree with it — a
        // hand-set flag does not outrank a lookup that has actually run.
        if tier != cfg.tier() {
            let _ = set_plan(tier);
        }
        return tier;
    }
    // Nothing to ask about: no account signed in and no checkout ever started.
    if account.is_none() && token.is_none() {
        return cfg.tier();
    }

    // An account this machine has not linked yet: register it, and let it pick
    // up a payment that arrived with nobody attached. Quiet and best-effort —
    // the lookup right below is what actually decides anything.
    //
    // Only ever this account's own checkout. `claim_checkout` points a token's
    // row at the caller, so claiming one held over from another sign-in is how
    // a subscription would change hands — the thing `link_account` refuses to
    // do by email and this must refuse to do by token.
    if let Some(account) = &account {
        if record.user_id.as_deref() != Some(account.sub.as_str()) {
            let _ = link(account).await;
            if let Some(token) = token.as_deref() {
                let _ = claim(token, account).await;
            }
        }
    }

    match fetch(account.as_ref(), token.as_deref()).await {
        Ok(status) => {
            let updated = Record {
                token,
                user_id: account.map(|a| a.sub),
                status: status.clone(),
                checked: Some(now),
            };
            // A failed write is not a reason to lie about the answer; the next
            // launch just asks again.
            let _ = updated.save();

            match verdict(&status) {
                // Active: the plan the row names, or Basic when the row is
                // older than tiers. The flag and the plan are one write.
                Some(true) => {
                    let tier = status.tier();
                    let _ = set_plan(tier);
                    tier
                }
                // A recorded no: clear the flag and the plan both.
                Some(false) => {
                    let _ = set_plan(None);
                    None
                }
                // Nothing to say — the flag and plan are the only evidence
                // there is, and this machine's own copy of them is kept.
                None => cfg.tier(),
            }
        }
        // Unreachable. Ride on the last good answer rather than demoting
        // somebody mid-flight — but only where that answer was about this
        // account.
        Err(_) => record
            .within_grace(now)
            .then(|| record.status.tier())
            .flatten()
            .or_else(|| cfg.tier()),
    }
}

/// What an answer from the lookup should do to the saved flag: `Some(active)`
/// to write it, `None` to leave whatever is there alone.
///
/// "No row" is not "not subscribed". Somebody who paid before any of this
/// existed has a hand-set flag and nothing in the table, and taking their star
/// away on the strength of an empty result would be the lookup overruling the
/// only evidence there is.
///
/// An empty result is the only thing that earns that silence, though, and the
/// distinction is the whole of this function. A row that came back saying
/// `pending` is not an absence — it is a checkout somebody started and did not
/// finish, and reading it as "say nothing" left the flag standing where the
/// service had in fact answered. That is how `craft configure` came to hand a
/// measurement id to an account that had never paid: the account's own
/// abandoned checkout was mistaken for having no evidence at all, and the
/// fallback behind it was a flag left in the config by a different account.
///
/// So the account flag is read as the answer it is. `subscribed` is kept by the
/// service across every payment attached to the account and recomputed
/// whenever one lands, renews, lapses or is adopted; `Some(false)` from it is
/// not silence but a no. Only a lookup that matched nothing at all — no row, so
/// no flag either — leaves the config alone.
fn verdict(status: &Status) -> Option<bool> {
    match status {
        s if s.is_active() => Some(true),
        // The account is known and its flag is down. An answer, and the one
        // the pending arm below used to swallow.
        s if s.subscribed == Some(false) => Some(false),
        // Nothing matched, so nothing to say: leave the flag as it is. An
        // older service that predates the account flag also lands here for a
        // checkout still in flight, which is the case the silence was for.
        s if s.is_pending() => None,
        s => Some(s.is_active()),
    }
}

/// Write the flag and plan the dashboard and the gates read.
///
/// Returns whether anything changed, so a check that finds what it expected can
/// stay quiet. The config round-trips whole; these are the only fields touched.
///
/// `None` clears both: a cancelled subscription is not a Pro, it is just a
/// subscriber who stopped.
pub fn set_plan(tier: Option<Tier>) -> Result<bool> {
    let mut cfg = crate::config::Config::load()?;
    if cfg.supporter == tier.is_some() && cfg.tier == tier {
        return Ok(false);
    }
    cfg.supporter = tier.is_some();
    cfg.tier = tier;
    cfg.save()?;
    Ok(true)
}

/// What the star says to somebody who has already paid.
///
/// Six lines rather than one. The old single thank-you was about us; these are
/// about them, and about the work they opened the dashboard to do.
///
/// The last one earns its place by answering the question a subscriber does
/// ask — whether this is still worth the line on the card — without naming a
/// number to do it. A competitor's price compiled into a binary cannot be
/// corrected without a release, so it would be wrong the first time they
/// change their pricing page and stay wrong on every installed copy. Price
/// comparisons belong on the site, where they can be edited.
pub const SUPPORTER_LINES: [&str; 6] = [
    "keep mining",
    "the vein runs deep",
    "go make the bars go up",
    "swing away",
    "this pickaxe is yours",
    "the same numbers, none of the invoice",
];

/// Keeps this digest distinct from any other use of the same token.
const LINE_DOMAIN: &[u8] = b"anacraft/supporter-line/v1";

/// The line shown when there is no account to derive one from.
///
/// An index rather than a fixed seed, which is where [`crate::avatar`]'s
/// pattern stops applying: a face nobody can read means nothing either way,
/// but this string is baked into the site's dashboard captures, so it is the
/// one line a prospect reads. That makes it an editorial decision, not
/// something to leave to the low byte of a digest.
const DEMO_LINE: usize = 5;

/// Pick a line for whoever is signed in, falling back to the fixed one when
/// there is no token to read.
///
/// Seeded off the refresh token the way [`crate::avatar`] seeds a face, so an
/// account keeps the same line on every machine it signs in from and a
/// different one from its neighbour. Stable rather than random per run on
/// purpose: a line that changed every thirty seconds would read as a ticker
/// rather than as something said to you.
pub fn supporter_line() -> &'static str {
    match crate::auth::Tokens::load() {
        Ok(Some(tokens)) => line_from_seed(tokens.refresh_token.as_bytes()),
        _ => demo_supporter_line(),
    }
}

/// The fixed line, for the demo and for the site's captures.
pub fn demo_supporter_line() -> &'static str {
    SUPPORTER_LINES[DEMO_LINE]
}

/// The number worn in the demo and in the site's captures. Low, because what
/// the preview is previewing is what an early Anacrafter sees.
pub const DEMO_FOUNDER: u32 = 7;

/// Derive a line from arbitrary bytes.
///
/// A digest rather than `seed[0] % 7`: the low byte of a refresh token is not
/// uniform, and the domain prefix means the same secret used elsewhere cannot
/// produce a matching value. Only the index reaches the screen.
fn line_from_seed(seed: &[u8]) -> &'static str {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest([LINE_DOMAIN, seed].concat());
    SUPPORTER_LINES[digest[0] as usize % SUPPORTER_LINES.len()]
}

/// The subscriber gate, and the sentence that gets an unsubscribed user
/// unstuck.
///
/// Shared by every command that is part of the subscription so the wording
/// exists once: `craft mcp` relays it through a locked tool, `craft watch`
/// bails with it. `command` names the caller, because "run this with --demo"
/// is only useful advice if it names the thing to run.
///
/// `tier` is the plan this machine is on — the cached answer from [`sync`] —
/// and `required` is the plan the command needs. Two shapes of refusal come
/// out of that: somebody with no subscription at all is told how to start one,
/// and somebody on a lower plan is told which plan has the thing they are
/// reaching for, because moving up is a different and smaller decision.
///
/// It is a soft gate either way. The answer it consults is a line of TOML in a
/// config anybody can edit, in a binary anybody can rebuild — the point is to
/// ask honestly, not to be unpickable.
pub fn gate(tier: Option<Tier>, required: Tier, command: &str) -> std::result::Result<(), String> {
    match tier {
        Some(have) if have.meets(required) => Ok(()),
        Some(have) => Err(upgrade(have, required, command)),
        None => Err(subscribe_first(command)),
    }
}

/// The ask for somebody who has not subscribed at all.
fn subscribe_first(command: &str) -> String {
    format!(
        "{command} is part of the Anacraft subscription.\n     \
         Run `craft subscribe` to start one — it writes the plan into {} \
         once the payment clears. Already subscribed on another machine? \
         `craft login` with the same Google account, then `craft subscribe --check`.\n     \
         `{command} --demo` runs on synthetic data and needs no subscription.",
        crate::config::Config::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "your config".into())
    )
}

/// The ask for somebody whose plan is real but short of what they reached for.
fn upgrade(have: Tier, required: Tier, command: &str) -> String {
    // What the higher plan's price buys, in the words the site uses on it.
    let worth_it = match required {
        Tier::Pro => "craft audit and Slack alerts",
        Tier::Elite => "craft mcp and the AI-tool integration",
        Tier::Basic => "everything in the plan you already have",
    };
    format!(
        "{command} is on Anacrafter {} ({}).\n     \
         It needs Anacrafter {} ({}), which adds {}.\n     \
         Run `craft subscribe --plan {}` to move up;\n     \
         the change is billed going forward, not charged again.",
        have.label(),
        have.monthly(),
        required.label(),
        required.monthly(),
        worth_it,
        required.name(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_account_keeps_the_same_line_on_every_machine() {
        assert_eq!(
            line_from_seed(b"1//0aRefreshToken"),
            line_from_seed(b"1//0aRefreshToken")
        );
    }

    #[test]
    fn a_different_account_gets_a_different_line() {
        // Not guaranteed for any given pair — there are seven lines — but
        // these two specific seeds must not collide, or the test below is
        // testing nothing.
        assert_ne!(
            line_from_seed(b"1//0aRefreshToken"),
            line_from_seed(b"1//0bRefreshToken")
        );
    }

    #[test]
    fn every_line_is_reachable() {
        // A picker that can only ever produce two of seven lines is a bug that
        // no single-seed assertion would catch.
        let mut seen = std::collections::BTreeSet::new();
        for n in 0..500u32 {
            seen.insert(line_from_seed(&n.to_le_bytes()));
        }
        assert_eq!(
            seen.len(),
            SUPPORTER_LINES.len(),
            "unreachable lines: {:?}",
            SUPPORTER_LINES
                .iter()
                .filter(|line| !seen.contains(*line))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_demo_line_is_fixed() {
        // The site's captures bake this in, so it must not depend on whose
        // machine regenerated them.
        assert_eq!(
            demo_supporter_line(),
            "the same numbers, none of the invoice"
        );
        assert!(
            DEMO_LINE < SUPPORTER_LINES.len(),
            "index is out of the table"
        );
    }

    #[test]
    fn no_line_is_wide_enough_to_break_a_narrow_panel() {
        // The box is one line in a panel that can be 80 columns; the star, the
        // word ANACRAFTER and the separator take about 20 of them.
        for line in SUPPORTER_LINES {
            assert!(line.len() <= 40, "too wide for the panel: {line:?}");
            assert_eq!(line.trim(), line, "padding belongs to the caller: {line:?}");
        }
    }

    fn account(sub: &str) -> Account {
        Account {
            sub: sub.to_string(),
            email: Some("me@example.com".to_string()),
        }
    }

    #[test]
    fn active_and_trialing_both_count() {
        for status in ["active", "trialing"] {
            let s = parse(&format!("[{{\"status\":\"{status}\"}}]")).unwrap();
            assert!(s.is_active(), "{status} should count");
            assert!(!s.is_pending());
        }
    }

    #[test]
    fn everything_stripe_calls_unpaid_is_not_active() {
        for status in ["canceled", "past_due", "unpaid", "incomplete_expired"] {
            let s = parse(&format!("[{{\"status\":\"{status}\"}}]")).unwrap();
            assert!(!s.is_active(), "{status} should not count");
            assert!(!s.is_pending(), "{status} is an answer, not a wait");
            assert_eq!(s.label(), status);
        }
    }

    #[test]
    fn no_row_reads_as_pending_rather_than_as_a_refusal() {
        // What an account that has never checked out gets back.
        for body in ["[]", "null"] {
            let s = parse(body).unwrap();
            assert!(s.is_pending(), "{body} should read as pending");
            assert!(!s.is_active());
            assert_eq!(s.label(), "pending");
        }
    }

    #[test]
    fn a_scalar_row_parses_the_same_as_a_table_row() {
        let table = parse("[{\"status\":\"active\",\"since\":\"2026-01-02T03:04:05Z\"}]").unwrap();
        let scalar = parse("{\"status\":\"active\",\"since\":\"2026-01-02T03:04:05Z\"}").unwrap();
        assert_eq!(table, scalar);
        assert!(table.since.is_some());
    }

    #[test]
    fn the_accounts_own_flag_outranks_the_row_the_lookup_returned() {
        // Two payments on one account: the query returned the dead one, but the
        // account is subscribed on the strength of the other.
        let s = parse("[{\"status\":\"canceled\",\"subscribed\":true}]").unwrap();
        assert!(s.is_active(), "the account flag was ignored");
        assert!(!s.is_pending());
        assert_eq!(s.label(), "canceled", "the row status is still reportable");

        // And the other way: a row that reads active against an account flag
        // that says otherwise.
        let stale = parse("[{\"status\":\"active\",\"subscribed\":false}]").unwrap();
        assert!(!stale.is_active());
    }

    #[test]
    fn a_service_that_does_not_send_the_flag_still_works() {
        // The shape the first migration answered with.
        let s = parse("[{\"status\":\"active\",\"since\":null}]").unwrap();
        assert_eq!(s.subscribed, None);
        assert!(
            s.is_active(),
            "the status has to decide when there is no flag"
        );
    }

    #[test]
    fn a_status_this_build_has_never_heard_of_survives_the_trip() {
        let s = parse("[{\"status\":\"paused\"}]").unwrap();
        assert!(!s.is_active());
        assert_eq!(s.label(), "paused", "unknown statuses get reported as-is");
    }

    #[test]
    fn a_junk_body_is_an_error_rather_than_a_silent_no() {
        // A captive portal's login page must never read as "not subscribed".
        assert!(parse("<html>nope</html>").is_err());
    }

    #[test]
    fn a_fresh_answer_is_not_asked_for_twice() {
        let now = Utc::now();
        let record = Record {
            token: None,
            user_id: Some("110147".into()),
            status: Status {
                status: "active".into(),
                since: None,
                subscribed: None,
                founder: None,
                tier: None,
            },
            checked: Some(now - Duration::hours(1)),
        };
        assert!(record.is_fresh(Some(&account("110147")), now));
        assert!(!record.is_fresh(Some(&account("110147")), now + Duration::hours(12)));
    }

    #[test]
    fn another_google_account_does_not_inherit_this_ones_answer() {
        let now = Utc::now();
        let record = Record {
            token: None,
            user_id: Some("110147".into()),
            status: Status {
                status: "active".into(),
                since: None,
                subscribed: None,
                founder: None,
                tier: None,
            },
            checked: Some(now),
        };
        assert!(!record.is_fresh(Some(&account("999")), now));
    }

    #[test]
    fn an_empty_lookup_never_demotes_somebody_who_paid_before_it_existed() {
        // A supporter from before the lookup has `supporter = true` set by hand
        // and no row anywhere. An empty answer must leave that alone.
        let pending = Status {
            status: "pending".into(),
            since: None,
            subscribed: None,
            founder: None,
            tier: None,
        };
        assert_eq!(verdict(&pending), None, "a hand-set flag was cleared");
        assert_eq!(verdict(&Status::default()), None, "an empty answer decided");

        // A real status is evidence, and evidence wins in both directions.
        let active = Status {
            status: "active".into(),
            since: None,
            subscribed: None,
            founder: None,
            tier: None,
        };
        assert_eq!(verdict(&active), Some(true));
        let gone = Status {
            status: "canceled".into(),
            since: None,
            subscribed: None,
            founder: None,
            tier: None,
        };
        assert_eq!(verdict(&gone), Some(false));
    }

    #[test]
    fn an_account_that_never_paid_does_not_inherit_the_flag_beside_it() {
        // The bug this is here for: `craft configure` created a property and
        // printed a measurement id for an account that had never subscribed.
        //
        // The account's own abandoned checkout was sitting in the table as
        // `pending`, and `subscribed` was down. The lookup had answered, and
        // answered no — but `pending` was read as "nothing recorded", so the
        // fallback behind it decided instead, and the fallback was
        // `supporter = true` left in the config by a different account that had
        // signed in on that machine earlier.
        let their_own_pending = parse("[{\"status\":\"pending\",\"subscribed\":false}]").unwrap();
        assert!(!their_own_pending.is_active());
        assert_eq!(
            verdict(&their_own_pending),
            Some(false),
            "a pending checkout on a known account let the flag stand"
        );

        // Same for the expired one it becomes about a day later.
        let expired = parse("[{\"status\":\"expired\",\"subscribed\":false}]").unwrap();
        assert_eq!(verdict(&expired), Some(false));

        // And the flag still wins where it is up: an in-flight second checkout
        // says nothing about the subscription already running.
        let second = parse("[{\"status\":\"pending\",\"subscribed\":true}]").unwrap();
        assert_eq!(verdict(&second), Some(true));

        // The protection this had to leave standing. A supporter from before
        // the lookup existed matches no row at all, so there is no account flag
        // either — and that silence is the only evidence there is.
        assert_eq!(
            verdict(&Status::default()),
            None,
            "an empty answer decided against somebody who paid"
        );
        assert_eq!(verdict(&parse("[]").unwrap()), None);
    }

    #[test]
    fn one_accounts_evidence_never_speaks_for_another() {
        // A token is 40 random characters and is not tied to the account that
        // minted it, and the record holding it outlives the sign-in that wrote
        // it. So a record naming somebody else must not be usable: not its
        // token, and not the config flag standing beside it.
        let paid = Record {
            token: Some("a".repeat(40)),
            user_id: Some("111".into()),
            status: Status {
                status: "active".into(),
                since: None,
                subscribed: Some(true),
                founder: Some(41),
                tier: None,
            },
            checked: Some(Utc::now()),
        };

        assert!(paid.speaks_for(Some(&account("111"))), "its own account");
        assert!(
            !paid.speaks_for(Some(&account("222"))),
            "another account inherited a subscription"
        );

        // No account signed in is not a mismatch — there is nothing to
        // contradict, and a hand-set flag on a build with no lookup behind it
        // is exactly this case.
        assert!(paid.speaks_for(None));

        // Nor is a record that names nobody: that is the anonymous checkout,
        // waiting for the first account that asks to adopt it.
        let anonymous = Record {
            user_id: None,
            ..paid.clone()
        };
        assert!(anonymous.speaks_for(Some(&account("222"))));
    }

    #[test]
    fn a_foreign_record_carries_no_grace_either() {
        // The offline path is the same question asked while the service is
        // unreachable, so it needs the same answer. Riding on "the last good
        // answer" is only right where that answer was about this account.
        let now = Utc::now();
        let paid = Record {
            token: Some("a".repeat(40)),
            user_id: Some("111".into()),
            status: Status {
                status: "active".into(),
                since: None,
                subscribed: Some(true),
                founder: None,
                tier: None,
            },
            checked: Some(now - Duration::days(1)),
        };

        // Still inside GRACE, which is the point: it is a live subscription
        // that simply cannot be re-checked right now.
        assert!(paid.within_grace(now));
        // And still not this account's.
        assert!(!paid.speaks_for(Some(&account("222"))));
    }

    #[test]
    fn an_abandoned_checkout_is_not_read_as_a_cancellation() {
        // Stripe expires an unpaid checkout session about a day after it opens,
        // and the webhook writes 'expired' onto the row the CLI claimed. That
        // is the absence of a payment, not the end of one: a supporter from
        // before the lookup has a hand-set flag and no live row, and starting a
        // checkout they never finished must not take their star away.
        let expired = parse("[{\"status\":\"expired\"}]").unwrap();
        assert!(!expired.is_active());
        assert!(
            expired.is_pending(),
            "an expired checkout decided something"
        );
        assert_eq!(verdict(&expired), None, "a hand-set flag was cleared");
        assert_eq!(expired.label(), "expired", "still reportable as itself");

        // The account flag still outranks it — an expired second checkout says
        // nothing about the subscription already running.
        let alongside = parse("[{\"status\":\"expired\",\"subscribed\":true}]").unwrap();
        assert!(alongside.is_active());
        assert!(!alongside.is_pending());

        // And Stripe's own `incomplete_expired` is a different word: that one
        // is a subscription that died, and it is an answer.
        let incomplete = parse("[{\"status\":\"incomplete_expired\"}]").unwrap();
        assert!(!incomplete.is_pending(), "a dead subscription is an answer");
        assert_eq!(verdict(&incomplete), Some(false));
    }

    #[test]
    fn a_cached_no_is_asked_again_every_launch() {
        // The state somebody is trying to leave: they have just paid, or just
        // signed in on a second machine. Caching it for twelve hours would mean
        // twelve hours of "why is nothing happening".
        let now = Utc::now();
        for status in ["pending", "canceled", "past_due"] {
            let record = Record {
                token: Some("tok".into()),
                user_id: Some("110147".into()),
                status: Status {
                    status: status.into(),
                    since: None,
                    subscribed: None,
                    founder: None,
                    tier: None,
                },
                checked: Some(now),
            };
            assert!(
                !record.is_fresh(Some(&account("110147")), now),
                "{status} was cached"
            );
        }
    }

    #[test]
    fn a_record_that_was_never_checked_is_never_fresh() {
        let record = Record {
            token: Some("tok".into()),
            ..Record::default()
        };
        assert!(!record.is_fresh(None, Utc::now()));
        assert!(!record.within_grace(Utc::now()));
    }

    #[test]
    fn an_unreachable_check_keeps_a_paid_subscriber_going_but_not_forever() {
        let now = Utc::now();
        let paid = |age: Duration| Record {
            token: None,
            user_id: Some("110147".into()),
            status: Status {
                status: "active".into(),
                since: None,
                subscribed: None,
                founder: None,
                tier: None,
            },
            checked: Some(now - age),
        };
        assert!(paid(Duration::days(3)).within_grace(now));
        assert!(!paid(Duration::days(30)).within_grace(now));

        // Somebody who was already cancelled gets no grace at all.
        let cancelled = Record {
            status: Status {
                status: "canceled".into(),
                since: None,
                subscribed: None,
                founder: None,
                tier: None,
            },
            checked: Some(now),
            ..Record::default()
        };
        assert!(!cancelled.within_grace(now));
    }

    #[test]
    fn the_checkout_url_carries_the_token_and_the_email() {
        assert_eq!(
            checkout_url("https://buy.stripe.com/abc", "tok", None),
            "https://buy.stripe.com/abc?client_reference_id=tok"
        );
        assert_eq!(
            checkout_url(
                "https://buy.stripe.com/abc?locale=en",
                "tok",
                Some("a+b@x.io")
            ),
            "https://buy.stripe.com/abc?locale=en&client_reference_id=tok\
             &prefilled_email=a%2Bb%40x.io"
        );
        // An account with no email on it must not append an empty parameter.
        assert!(!checkout_url("https://buy.stripe.com/abc", "tok", Some("")).contains("prefilled"));
    }

    #[test]
    fn minted_tokens_are_long_and_never_repeat() {
        let a = mint_token();
        assert_eq!(a.len(), 40);
        assert_ne!(a, mint_token());
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn a_record_written_by_an_older_build_still_loads() {
        // Every field is optional on the way in; a half-written file is a
        // cold cache, not a broken install.
        let record: Record = serde_json::from_str("{\"token\":\"tok\"}").unwrap();
        assert_eq!(record.token.as_deref(), Some("tok"));
        assert!(record.checked.is_none());
        assert!(record.status.is_pending());
    }

    // ------------------------------------------------------------- the plans ---

    #[test]
    fn a_live_row_names_its_plan_and_an_older_one_reads_as_basic() {
        // The row the webhook keeps now carries the tier the price implies.
        let elite = parse("[{\"status\":\"active\",\"tier\":\"elite\"}]").unwrap();
        assert_eq!(elite.tier(), Some(Tier::Elite));

        // A row from before tiers says nothing about the plan, and that is
        // Basic: it is what the $2.99 subscription always was, so nothing
        // already paid is ever downgraded by an older record.
        let legacy = parse("[{\"status\":\"active\"}]").unwrap();
        assert_eq!(legacy.tier(), Some(Tier::Basic));
    }

    #[test]
    fn a_subscription_that_is_not_live_has_no_plan_at_all() {
        // A cancelled Elite is not Pro. The plan travels with the payment, so a
        // lapsed one stops answering for anything.
        let gone = parse("[{\"status\":\"canceled\",\"tier\":\"elite\"}]").unwrap();
        assert_eq!(gone.tier(), None);
    }

    #[test]
    fn every_plan_contains_the_ones_below_it() {
        assert!(Tier::Basic.meets(Tier::Basic));
        assert!(!Tier::Basic.meets(Tier::Pro));
        assert!(Tier::Pro.meets(Tier::Basic));
        assert!(Tier::Pro.meets(Tier::Pro));
        assert!(!Tier::Pro.meets(Tier::Elite));
        assert!(Tier::Elite.meets(Tier::Basic));
        assert!(Tier::Elite.meets(Tier::Elite));
    }

    #[test]
    fn the_plan_names_and_prices_are_the_sites() {
        assert_eq!(Tier::Basic.name(), "basic");
        assert_eq!(Tier::Pro.name(), "pro");
        assert_eq!(Tier::Elite.name(), "elite");
        assert_eq!(Tier::Pro.label(), "Anacrafter Pro");
        assert_eq!(Tier::Elite.label(), "Anacrafter Elite");
        assert_eq!(Tier::Basic.monthly(), "$2.99/month");
        assert_eq!(Tier::Pro.monthly(), "$5.99/month");
        assert_eq!(Tier::Elite.monthly(), "$9.99/month");
        assert_eq!(Tier::Pro.monthly(), "$5.99/month");
        assert_eq!(Tier::Pro.name(), Tier::parse("pro").unwrap().name());
    }

    #[test]
    fn an_unrecognised_tier_word_is_not_a_plan() {
        assert_eq!(Tier::parse("platinum"), None);
        assert_eq!(Tier::parse("BASIC"), None, "case is significant");
    }

    #[test]
    fn a_basic_subscriber_is_told_what_pro_adds() {
        let err = gate(Some(Tier::Basic), Tier::Pro, "Slack alerts").unwrap_err();
        assert!(err.contains("Anacrafter Pro"), "got {err}");
        assert!(err.contains("$5.99"), "no price in the ask: {err}");
        assert!(
            err.contains("craft subscribe --plan pro"),
            "no way up: {err}"
        );
    }

    #[test]
    fn a_known_plan_and_a_new_one_pass_their_own_gate() {
        assert!(gate(Some(Tier::Basic), Tier::Basic, "craft watch").is_ok());
        assert!(gate(Some(Tier::Pro), Tier::Pro, "craft watch").is_ok());
        assert!(gate(Some(Tier::Elite), Tier::Elite, "craft mcp").is_ok());
        assert!(gate(None, Tier::Basic, "craft watch").is_err());
    }
}
