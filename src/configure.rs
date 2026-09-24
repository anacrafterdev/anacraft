//! `craft configure <domain>` — the setup guide, run as a command.
//!
//! `docs/setup-ga4.html` is the most-read page on the site, and steps 01–03 of
//! it are the reason: create a property, add a web data stream, copy the
//! measurement id into a snippet. None of that is a decision. It is four
//! screens of console navigation standing between somebody and their first
//! number, and every one of those screens is an Admin API call.
//!
//! So this does the three steps and prints the tag with the real id already in
//! it. What it deliberately does not do is guess: it will reuse a property
//! that already measures the domain rather than quietly create a second one,
//! and it stops and asks when the account is ambiguous.
//!
//! This is the only command in anacraft that writes anything to a Google
//! account, and it asks for the permission to do so itself — see
//! `auth::ensure_scope` and `docs/oauth-scopes.md`.
//!
//! MCP's `configure_site` tool runs this same core — see [`setup`] — with the
//! browser consent swapped for a check of the stored grant, because a client's
//! subprocess has no browser to open. The result comes back as data; how and
//! where it is said is the caller's, a terminal's panels or a tool's JSON.
//!
//! Which is also why it is the command that asks for the subscription: the
//! guide is the most-read page on the site, and this is its first line, so the
//! people running this are the people who are actually here. `Paywall` below
//! is the whole of that, and the shape of it is that the ask arrives on the
//! page the sign-in already ends on rather than in a tab of its own.
//!
//! `craft delete` is the other half, and deliberately the asymmetric one. On
//! its own it only forgets a property locally and hands the console the
//! destructive click. `--all` is the second half of that asymmetry rather than
//! the end of it: a property this command created in one line should not need
//! four console screens to take back, so the flag exists — but it has to be
//! typed, it is never what a bare `craft delete` does, and what it reaches for
//! is Google's soft delete, which leaves the property restorable from the
//! account's own trash for 35 days. The grant is one scope wide and the code
//! keeps using less of it than the scope allows; see `docs/oauth-scopes.md`.

use anyhow::{bail, Context, Result};

use crate::auth::{Cta, Landing, Tokens, SCOPE_EDIT};
use crate::config::Config;
use crate::ga::{Account, Ga, Property, WebStream};
use crate::render::{bold, dim, paint, panel_bottom, panel_top};
use crate::theme::{glyph, ore};

/// How many properties to search for an existing stream before giving up on
/// the reuse check. An account with more properties than this is a reseller's,
/// and the scan is one API call each.
const SCAN_LIMIT: usize = 40;

pub struct Options {
    /// GA4 account to create under, when the login can see more than one.
    pub account: Option<String>,
    /// IANA reporting time zone. Detected from the machine when absent.
    pub timezone: Option<String>,
    /// ISO 4217 reporting currency.
    pub currency: String,
}

pub async fn run(domain: &str, opts: Options) -> Result<()> {
    let host = host_of(domain)?;
    let uri = format!("https://{host}");

    let ga = Ga::new()?;

    // Everything above this line is local, so somebody who stops at either of
    // the two screens below — Google's, or Stripe's — has changed nothing.
    let cold = Tokens::load()?.is_none();
    let mut paywall = Paywall::open(cold).await?;

    // A machine that has never signed in needs one browser trip, not two, so
    // the cold start asks for both scopes at once — and carries the
    // subscription ask, when one is owed, on the page that trip ends on.
    //
    // The order is the fix for a subscriber being asked to subscribe. Before
    // the trip there is no account, so the page could only be chosen on what
    // the machine happened to say about itself; after it there is one, and
    // `reconsider` asks the service about it while the tab is still waiting.
    // So an Anacrafter on a new laptop is told they are in, and never sees a
    // button offering them a second subscription.
    if cold {
        let why = format!(
            "setting up {host} needs permission to add a property to your Analytics account"
        );
        let consented = ga.auth().ensure_scope(SCOPE_EDIT, &why).await?;
        paywall = paywall.reconsider().await?;
        consented.show(&paywall.landing());
    }

    if !paywall.settle(&host).await? {
        // The wait has already said how to pick this up, and nothing was
        // created to pick up from.
        return Ok(());
    }

    // From here on it is the same work whether a person at a terminal is
    // asking or an assistant over MCP is: [`setup`] holds both together, and
    // this run asks for any missing consent on the spot, because there is a
    // browser to do it in — `Consent::Ask` is the whole difference from the
    // MCP call.
    let setup = setup(&ga, &host, opts, Consent::Ask).await?;
    save(&setup.property.id, &host)?;

    println!();
    print_setup(&setup, &host, &uri);
    Ok(())
}

/// The whole of [`setup`], retold in the terminal's own voice.
fn print_setup(setup: &Setup, host: &str, uri: &str) {
    if let Some(note) = &setup.note {
        println!("  {}", dim(note));
    }

    match setup.action {
        // Running this twice is the normal way to get the tag back. Creating a
        // second property for the same site would split the numbers in two,
        // and nothing would say so until a week of data had gone to the wrong
        // one.
        SetupAction::Reused => {
            println!(
                "  {} {} is already measured by {} {}",
                paint("✓", ore::emerald()),
                bold(host),
                bold(&paint(&setup.property.name, ore::diamond())),
                dim(&format!("({})", setup.property.id)),
            );
            println!("  {}\n", dim("nothing was created — here is its tag again"));
        }

        // Reaching either arm below means something was about to be created,
        // and that was the first moment anything was.
        SetupAction::Finished => {
            println!(
                "  {} finishing {} {}",
                glyph::PICKAXE,
                bold(&paint(host, ore::diamond())),
                dim(&format!(
                    "({}, created earlier but never given a stream)",
                    setup.property.id
                )),
            );
        }

        SetupAction::Created => {
            println!(
                "  {} creating a property for {} in {}",
                glyph::PICKAXE,
                bold(host),
                dim(setup.account.as_deref().unwrap_or("your Analytics account")),
            );
            println!(
                "  {} property {} {}",
                paint("✓", ore::emerald()),
                bold(&paint(host, ore::diamond())),
                dim(&format!(
                    "({}, reporting in {})",
                    setup.property.id,
                    setup.timezone.as_deref().unwrap_or("UTC"),
                )),
            );
        }
    }

    if !matches!(setup.action, SetupAction::Reused) {
        println!(
            "  {} web stream {} {}\n",
            paint("✓", ore::emerald()),
            bold(&setup.stream.measurement_id),
            dim(&format!("({uri})")),
        );
    }

    if setup.timezone_fell_back {
        println!(
            "  {}",
            dim(
                "could not read this machine's time zone; reporting in UTC — \
                 re-run with --timezone to change it"
            )
        );
    }

    print_tag(&setup.stream.measurement_id);
    print_next(host);
}

// ------------------------------------------------------------------- setup ---

/// Whether a write may open its own consent flow, or must work from the grant
/// already on hand.
///
/// The CLI can open a browser; an MCP subprocess is inside a client and cannot.
/// That split is the whole reason the work below is shared — the same calls
/// serve a terminal and a tool, and only the consent differs.
pub(crate) enum Consent {
    /// A person is watching: a missing grant opens the consent screen.
    Ask,
    /// No browser available. A missing grant is an error that names the
    /// terminal command that fixes it.
    HeldOnly,
}

/// What a run changed on Google's side, for the caller to say in its own voice
/// — a terminal's panels, an MCP tool's JSON.
pub(crate) struct Setup {
    pub property: Property,
    pub stream: WebStream,
    pub action: SetupAction,
    /// The time zone the property reports in, when this run chose it.
    pub timezone: Option<String>,
    /// True when this machine's time zone could not be read and the property
    /// was created reporting in UTC in its place.
    pub timezone_fell_back: bool,
    /// The account the property lives in, when this run created it.
    pub account: Option<String>,
    /// Anything discovery learned that the caller should say out loud.
    pub note: Option<String>,
}

pub(crate) enum SetupAction {
    /// A property already measured this domain; nothing was created.
    Reused,
    /// The property existed but had no stream; the stream was created just now.
    Finished,
    /// Both the property and its web stream were created just now.
    Created,
}

/// The whole of `craft configure`, without the terminal's telling of it.
///
/// Reusing an existing measurement, creating a property and its web stream,
/// deciding where and in what time zone — every decision and every write lives
/// here, shared by the CLI's [`run`] and the MCP `configure_site` tool, so both
/// see exactly the same Google-shaped behaviour. What differs is the two ends'
/// [`Consent`] and how they retell the result.
pub(crate) async fn setup(ga: &Ga, host: &str, opts: Options, consent: Consent) -> Result<Setup> {
    let uri = format!("https://{host}");

    // Discovery runs on the read grant a login already has, which is why a
    // re-run — the normal way to get the tag back — needs no consent screen.
    let (existing, note) = find_existing(ga, host).await?;

    match existing {
        // Running this twice is the normal way to get the tag back, and there
        // is nothing to write: no consent to ask for either.
        Some(Existing::Measured(property, stream)) => Ok(Setup {
            property,
            stream,
            action: SetupAction::Reused,
            timezone: None,
            timezone_fell_back: false,
            account: None,
            note,
        }),

        // Reaching either arm below means something is about to be created,
        // and that is the first moment anything is — and the moment the write
        // scope is asked for. Somebody who only wanted their tag back was
        // served above, within access on hand.
        Some(Existing::Unfinished(property)) => {
            consent_to_write(ga, host, &consent).await?;
            let stream = finish_stream(ga, &property, host, &uri).await?;
            Ok(Setup {
                property,
                stream,
                action: SetupAction::Finished,
                timezone: None,
                timezone_fell_back: false,
                account: None,
                note,
            })
        }

        None => {
            consent_to_write(ga, host, &consent).await?;
            let account = pick_account(ga, opts.account.as_deref()).await?;
            let (timezone, timezone_fell_back) = match opts.timezone {
                Some(tz) => (tz, false),
                None => match local_timezone() {
                    Some(tz) => (tz, false),
                    // No TZ and no localtime worth reading: report in UTC and
                    // say so, rather than invent a plausible-looking zone.
                    None => ("UTC".to_string(), true),
                },
            };

            let property = ga
                .create_property(&account, host, &timezone, &opts.currency)
                .await
                .with_context(|| format!("creating a property for {host}"))?;
            let stream = finish_stream(ga, &property, host, &uri).await?;

            Ok(Setup {
                property,
                stream,
                action: SetupAction::Created,
                timezone: Some(timezone),
                timezone_fell_back,
                account: Some(account.name),
                note,
            })
        }
    }
}

/// The web stream that makes a property measurable.
///
/// The property exists from here on. A failure below leaves it behind rather
/// than rolling back — an empty property costs nothing, and deleting one on
/// somebody's behalf because a second call failed is the more surprising
/// outcome. Re-running is safe: the next run recognises that property as
/// unfinished and adds the stream to it.
async fn finish_stream(ga: &Ga, property: &Property, host: &str, uri: &str) -> Result<WebStream> {
    ga.create_web_stream(&property.id, host, uri)
        .await
        .with_context(|| {
            format!(
                "property {} was created, but adding its web stream failed — \
                 re-run this command to finish it",
                property.id
            )
        })
}

/// Ask for the write scope, to the depth `consent` permits.
async fn consent_to_write(ga: &Ga, host: &str, consent: &Consent) -> Result<()> {
    let why =
        format!("setting up {host} needs permission to add a property to your Analytics account");
    match consent {
        // There is a browser nearby, so this is the consent screen — the same
        // page every other write in the CLI lands on.
        Consent::Ask => {
            ga.auth()
                .ensure_scope(SCOPE_EDIT, &why)
                .await?
                .show(&crate::auth::GRANTED);
            Ok(())
        }
        // A client's subprocess is not a place to open a browser. The write is
        // allowed on the grant already stored, or not at all — and the refusal
        // names the terminal command that refreshes it.
        Consent::HeldOnly => match ga.auth().tokens()? {
            Some(tokens) if tokens.granted(SCOPE_EDIT) => Ok(()),
            Some(_) => bail!(
                "stored credentials don't include the write scope — run `craft login` once \
                 in a terminal to refresh the grant, then restart the MCP client"
            ),
            None => bail!(
                "not logged in — run `craft login` in a terminal, then restart the MCP client"
            ),
        },
    }
}

// ----------------------------------------------------------------- paywall ---

/// The subscription, and the one place `craft configure` asks for it.
///
/// This command is where the most people are: `docs/setup-ga4.html` is the
/// most-read page on the site and this is its first line, so somebody running
/// this is somebody who has arrived. Asking three commands later, at `craft
/// watch`, is asking the ones who never got that far.
///
/// Where the ask lands is the rest of it. A cold start is about to open a
/// browser for Google's consent screen anyway, so the checkout rides back on
/// the page that trip ends on: one tab, one trip, and the terminal is already
/// sitting on a poll by the time somebody reaches for their card. What comes
/// back is the same `supporter = true` every other machine gets, written by
/// the same code path `craft subscribe` ends in.
///
/// And the page is chosen last. The tab is held open across the token exchange
/// so that `reconsider` can ask the service about the account that just signed
/// in — which is the only way an ask can be withheld from somebody who has
/// already paid, since before the trip there is nobody to ask about.
///
/// It is a paywall in the honest sense and not in the other one. Nothing is
/// created in Analytics before the payment clears, nothing is charged if it
/// never does, and the flag it ends in is a line of TOML in a config anybody
/// can edit — see `license::gate` for the same argument made once.
enum Paywall {
    /// Already an Anacrafter. Nothing to ask, and nothing to wait for.
    Paid,
    /// Not yet.
    Owed {
        /// Minted before the browser opens, because the page carrying the
        /// checkout has to have a URL to put on the button and a cold start
        /// has no account yet to key one to. The tie to the Google account is
        /// made after the fact by `claim`, and the webhook writes the row from
        /// this token either way, so neither order loses the payment.
        token: String,
        checkout: String,
        /// The line under the button. Held here so the price is quoted from
        /// the one place that knows it.
        note: String,
        /// Whether the ask is riding on a consent screen that was going to be
        /// shown regardless. False means there is no page to put it on —
        /// somebody already signed in — so the checkout gets its own tab.
        on_consent: bool,
    },
}

/// Whether this machine is already an Anacrafter, asked with the care the
/// moment deserves — because the next thing that happens if the answer is no
/// is somebody being sent to pay.
///
/// Two questions, cheapest first. `sync` answers from the cache when it can,
/// which is the common case and costs nothing. What it cannot know about is the
/// payment made in a browser a few minutes ago — on the pricing page, under
/// the same email, quite possibly on another machine — because a paid-up cache
/// is trusted for hours and the account only picks up an unattached payment
/// when its email is registered again. `license::already_paid` does both
/// halves over the wire, and says nothing out loud when it finds nothing.
async fn already_an_anacrafter() -> Result<bool> {
    if crate::license::sync(&Config::load()?).await.is_some() {
        return Ok(true);
    }

    let account = crate::auth::Auth::account().ok().flatten();
    let record = crate::license::Record::load();
    // Only ever this account's own checkout: a token held over from another
    // sign-in on this machine would answer about their subscription. See
    // `license::Record::speaks_for`.
    let mine = record.speaks_for(account.as_ref());
    let token = record.token.filter(|_| mine);

    Ok(
        crate::license::already_paid(account.as_ref(), token.as_deref())
            .await
            .is_some(),
    )
}

impl Paywall {
    /// Ask where this machine stands, before anything opens.
    async fn open(cold: bool) -> Result<Paywall> {
        // A cold start has nobody to ask about yet. Whatever the flag says
        // here is about the machine and not about the person — the machine
        // could be a laptop that was handed on, or one where somebody else's
        // sign-in came and went — so the question is deferred to
        // [`Paywall::reconsider`], which asks it once the account is known and
        // while the browser tab is still open to be told the answer.
        if !cold && already_an_anacrafter().await? {
            return Ok(Paywall::Paid);
        }

        let token = crate::license::mint_token();
        // Known on a warm start and not on a cold one, which is the whole of
        // why it is an `Option`: prefilling it is what makes the Stripe
        // customer match the Google account without anybody typing an address
        // twice, and a cold start gets that from `link_account` instead.
        let email = crate::auth::Auth::account()
            .ok()
            .flatten()
            .and_then(|a| a.email);

        // Where the button goes depends on whether the ask has been made yet.
        //
        // On a cold start it has: the page Google hands back carries a button
        // captioned "Become an Anacrafter" with the price and "cancel any
        // time" on the line under it, and pressing it *is* the answer — so it
        // goes straight to the card field. Putting a page of plans in between
        // re-asks a question somebody just answered.
        //
        // A warm start has no such page. The tab opens out of nowhere, from a
        // command typed to create a property, and a tab that opens onto a card
        // field is a decision nobody was asked for. That one gets the pricing
        // page, which explains itself before it asks for anything.
        let link = if cold {
            crate::SUBSCRIBE_URL
        } else {
            crate::PRICING_URL
        };

        Ok(Paywall::Owed {
            checkout: crate::license::checkout_url(link, &token, email.as_deref()),
            token,
            note: format!(
                "{} · cancel any time · the terminal is already waiting",
                crate::price_line()
            ),
            on_consent: cold,
        })
    }

    /// Ask again, now that there is somebody to ask about.
    ///
    /// The cold start's `open` ran before the sign-in, so its answer was about
    /// the machine. This is the same question put once the OAuth trip has come
    /// back with an account — and it is asked before the browser has been told
    /// anything, which is the whole point: an Anacrafter on a second laptop
    /// gets the plain "you're in" page and the property they came for, not a
    /// button inviting them to pay twice.
    ///
    /// A `Paid` cannot become `Owed` here. Nothing about a subscription that
    /// exists is re-litigated by a command that only wants to create a
    /// property; the one direction this moves in is towards asking for less.
    async fn reconsider(self) -> Result<Paywall> {
        if matches!(self, Paywall::Paid) || already_an_anacrafter().await? {
            return Ok(Paywall::Paid);
        }

        // The button's URL was built before the trip, when a cold start had no
        // account to read an address off. It has one now — that is what the
        // trip just returned — and this is the last moment before the page
        // goes out, so the URL is rebuilt carrying it.
        //
        // Prefilling is what makes the Stripe customer match the Google
        // account without anybody typing an address twice, which is the whole
        // of how a payment finds its way back to the right subscription. A
        // cold start used to give that up and rely on `link_account` adopting
        // the row afterwards by whatever was typed into Stripe; it does not
        // have to any more.
        let Paywall::Owed {
            token,
            checkout,
            note,
            on_consent,
        } = self
        else {
            return Ok(self);
        };

        let email = crate::auth::Auth::account()
            .ok()
            .flatten()
            .and_then(|a| a.email)
            .filter(|email| !email.is_empty());

        Ok(Paywall::Owed {
            checkout: match &email {
                Some(_) => crate::license::checkout_url(
                    if on_consent {
                        crate::SUBSCRIBE_URL
                    } else {
                        crate::PRICING_URL
                    },
                    &token,
                    email.as_deref(),
                ),
                // Nothing learned, nothing to rebuild.
                None => checkout,
            },
            token,
            note,
            on_consent,
        })
    }

    /// What to leave the browser looking at once consent comes back.
    fn landing(&self) -> Landing<'_> {
        match self {
            // Deliberately the same shape of page `craft login` ends on: a
            // sentence, nothing to click, and the work already moving in the
            // terminal behind it. It names the subscription because that is
            // the thing somebody was braced to be asked about — being told it
            // is already handled is the reassuring half of the same fact.
            Paywall::Paid => Landing::plain(
                "You're in",
                "anacraft has the permission it needs, and this account is already an \
                 Anacrafter — nothing to pay for and nothing to paste. The terminal is \
                 setting the property up now; you can close this tab and watch it.",
            ),
            // Deliberately no `redirect`: this page is an ask, and a page
            // that asks something has no business moving on by itself.
            Paywall::Owed { checkout, note, .. } => Landing {
                title: "One thing left",
                redirect: None,
                body: "anacraft has the permission it needs. Creating the property is part of \
                       the Anacrafter subscription — start one here and it comes back to the \
                       terminal on its own, with nothing to paste. Already an Anacrafter? The \
                       terminal has picked that up by now; close this tab and let it.",
                cta: Some(Cta {
                    label: "Become an Anacrafter",
                    url: checkout,
                    note,
                }),
            },
        }
    }

    /// Wait for the payment, and say whether it arrived.
    ///
    /// `false` is not an error: somebody who closed the tab has an untouched
    /// Analytics account and an uncharged card, and the message on the way out
    /// is the same command they just ran.
    async fn settle(self, host: &str) -> Result<bool> {
        let Paywall::Owed {
            token,
            checkout,
            on_consent,
            ..
        } = self
        else {
            return Ok(true);
        };

        // No re-check here any more. `reconsider` asked once the account was
        // known and before the page went out, which is both earlier and the
        // only moment at which the answer could still change what the browser
        // was shown. Asking a third time would only cost a request and risk
        // saying "subscription found" underneath a button that says otherwise.

        println!(
            "\n  {} {} is part of the Anacrafter subscription  ·  {}\n",
            paint(glyph::STAR, ore::gold()),
            bold("craft configure"),
            crate::price_line(),
        );
        println!("  {}\n", bold(&checkout));
        if on_consent {
            println!(
                "  {}\n",
                dim("the tab Google just handed back has the same button on it")
            );
        } else {
            // Signed in already, so there was no consent screen to fold the
            // ask into and this is the tab that carries it.
            let _ = open::that(&checkout);
        }

        let account = crate::auth::Auth::account()?;

        if crate::license::project().is_none() {
            // A build with no subscription service behind it — one somebody
            // compiled themselves. There is nothing to poll, so say where the
            // flag lives rather than spin against nobody.
            println!(
                "  once it's active, set {} in {}\n",
                bold("supporter = true"),
                dim(&Config::path()?.display().to_string()),
            );
            return Ok(false);
        }

        // Claim the row before the payment lands, so the webhook has something
        // to fill in. Best-effort, the way `craft subscribe` has it: a claim
        // that will not go through is not worth blocking a payment over, and
        // only the tie to the Google account is lost.
        if let Some(account) = &account {
            if crate::license::claim(&token, account).await.is_err() {
                println!(
                    "  {}\n",
                    dim("could not record the checkout — it will still be picked up by token")
                );
            }
        }
        crate::license::Record {
            token: Some(token.clone()),
            user_id: account.as_ref().map(|a| a.sub.clone()),
            ..Default::default()
        }
        .save()?;

        // Re-running is the way back in, and it is the same command either
        // way: nothing was created, so the second run starts where this one
        // stopped rather than beside it.
        crate::wait_for_payment(account.as_ref(), &token, &format!("craft configure {host}")).await
    }
}

// ------------------------------------------------------------------ lookup ---

/// What the account already has for this domain.
pub(crate) enum Existing {
    /// A property with a web stream pointing at the domain. Nothing to do but
    /// print its tag.
    Measured(Property, WebStream),
    /// A property named after the domain with no web stream on it — what a run
    /// that created the property and then failed leaves behind. Finishing it is
    /// what the failure told the user to do, and doing so must not make a
    /// second property.
    Unfinished(Property),
}

/// What already exists for `host`, if anything, plus anything discovery
/// learned that the caller should say.
///
/// The note travels as data rather than printing, because a caller may have no
/// terminal to print to — see [`setup`] and the MCP `configure_site` tool.
///
/// Properties named after the domain are checked first, because that is what
/// this command names them, so the common re-run costs one extra call rather
/// than one per property.
pub(crate) async fn find_existing(
    ga: &Ga,
    host: &str,
) -> Result<(Option<Existing>, Option<String>)> {
    let mut properties = ga.properties().await?;
    properties.sort_by_key(|p| p.name.to_lowercase() != host);

    let total = properties.len();
    let mut unfinished = None;

    for property in properties.into_iter().take(SCAN_LIMIT) {
        // A property this login cannot read streams on is not a match; it is
        // also not a reason to fail, so skip it.
        let Ok(streams) = ga.web_streams(&property.id).await else {
            continue;
        };

        if let Some(stream) = streams
            .iter()
            .find(|s| host_of(&s.default_uri).is_ok_and(|h| h == host))
        {
            // A live stream beats an empty property, wherever each was found,
            // so this returns immediately and the fallback below never wins
            // over a real match.
            return Ok((
                Some(Existing::Measured(
                    property,
                    WebStream {
                        name: stream.name.clone(),
                        measurement_id: stream.measurement_id.clone(),
                        default_uri: stream.default_uri.clone(),
                    },
                )),
                None,
            ));
        }

        // Only an exact name match, and only the first: adding a stream to
        // some unrelated property because it happened to be empty would be a
        // change nobody asked for.
        if unfinished.is_none() && streams.is_empty() && property.name.to_lowercase() == host {
            unfinished = Some(property);
        }
    }

    // An account larger than the scan is worth knowing about: an empty property
    // past the scan limit would be finished as if nothing existed.
    let note = if unfinished.is_none() && total > SCAN_LIMIT {
        Some(format!(
            "checked the first {SCAN_LIMIT} of {total} properties for an existing \
             stream on this domain"
        ))
    } else {
        None
    };
    Ok((unfinished.map(Existing::Unfinished), note))
}

/// Which account to create in: the named one, the only one, or a question.
pub(crate) async fn pick_account(ga: &Ga, wanted: Option<&str>) -> Result<Account> {
    let accounts = ga.accounts().await?;

    if accounts.is_empty() {
        // Creating the *account* is a step this could take and does not.
        // `accounts.provisionAccountTicket` is in the same API under the same
        // `analytics.edit` scope, so there is no permission in the way — but
        // it works by handing back a ticket to put in a Terms of Service URL,
        // which means the person ends up on Google's page accepting Google's
        // terms regardless. Sending them straight there is the same trip with
        // one less moving part, and keeps this command's whole relationship
        // with somebody's Analytics account down to two creates.
        bail!(
            "this Google account has no Analytics account to create a property in.\n  \
             Create one at https://analytics.google.com/analytics/web/#/provision \
             (it takes a minute, and accepting the terms has to happen there),\n  \
             then run this again."
        );
    }

    if let Some(wanted) = wanted {
        let needle = wanted.trim().trim_start_matches("accounts/");
        return accounts
            .into_iter()
            .find(|a| a.id == needle || a.name.eq_ignore_ascii_case(needle))
            .with_context(|| format!("no Analytics account matching {wanted}"));
    }

    if accounts.len() == 1 {
        return Ok(accounts.into_iter().next().expect("length checked"));
    }

    // Picking one for somebody would put the property — and its data retention,
    // and who can see it — in a place they did not choose.
    let list = accounts
        .iter()
        .map(|a| format!("\n    {}  {}", a.id, a.name))
        .collect::<String>();
    bail!(
        "this login can see {} Analytics accounts, so pick the one to create in:{list}\n\n  \
         then: craft configure <domain> --account <id>",
        accounts.len()
    );
}

// ------------------------------------------------------------------ output ---

/// The gtag.js snippet, with the id already in both places it belongs.
///
/// Both places is the point. The single most common broken install is the id
/// pasted into the `src` and left as a placeholder in the `config` call, or the
/// reverse — the tag loads, nothing is recorded, and the console says only that
/// no data has been received.
///
/// The id is painted, and nothing else is: ANSI codes are not part of the text
/// a terminal copies, so this stays paste-safe, and `paint` drops them anyway
/// the moment stdout is not a TTY.
fn tag(measurement_id: &str) -> String {
    let id = paint(measurement_id, ore::gold());
    format!(
        "  {}\n  \
         <script async src=\"https://www.googletagmanager.com/gtag/js?id={id}\"></script>\n  \
         <script>\n    \
         window.dataLayer = window.dataLayer || [];\n    \
         function gtag(){{dataLayer.push(arguments);}}\n    \
         gtag('js', new Date());\n    \
         gtag('config', '{id}');\n  \
         </script>",
        dim("<!-- Google tag (gtag.js) -->"),
    )
}

fn print_tag(measurement_id: &str) {
    println!("{}\n", panel_top("PASTE THIS IN <HEAD>"));
    println!("{}\n", tag(measurement_id));
    println!("{}\n", panel_bottom());
    println!(
        "  {}",
        dim("on every page, and only once — a second GA4 tag doubles every number")
    );
}

/// The gtag.js snippet with the id in both places it belongs, bare — no panel,
/// no paint, no lead-in spaces.
///
/// Same bytes the terminal's [`tag`] tucks into its panel, for the callers
/// that have no terminal: the MCP `configure_site` tool carries this in its
/// JSON, and an assistant hands it to the site's owner to paste.
pub(crate) fn tag_snippet(measurement_id: &str) -> String {
    format!(
        "<!-- Google tag (gtag.js) -->\n\
         <script async src=\"https://www.googletagmanager.com/gtag/js?id={measurement_id}\"></script>\n\
         <script>\n\
         window.dataLayer = window.dataLayer || [];\n\
         function gtag(){{dataLayer.push(arguments);}}\n\
         gtag('js', new Date());\n\
         gtag('config', '{measurement_id}');\n\
         </script>"
    )
}

/// The snippet wrapped in instructions, for handing to the agent that built
/// the site rather than to the person who owns it.
///
/// Lovable, v0 and Bolt all take a paste in a chat box, and all three will
/// happily improve on a tag they are handed. The three ways they do it are
/// named here and ruled out: an npm wrapper instead of the tag, a second tag
/// beside the one already in the project, and a hand-rolled page view on route
/// changes — which GA4's enhanced measurement already sends from history
/// events, so adding one counts every navigation twice.
///
/// The tag is embedded rather than described, so there is exactly one place in
/// this binary that knows what a gtag.js snippet looks like.
pub(crate) fn tag_prompt(measurement_id: &str) -> String {
    format!(
        "Add Google Analytics 4 to this app.\n\n\
         Paste this tag into index.html as the last thing inside <head>, \
         exactly as written:\n\n\
         {tag}\n\n\
         Rules:\n\
         - Keep the measurement ID {measurement_id} exactly as it is, in both \
         of the places it appears.\n\
         - Do not install an analytics npm package or a React wrapper. The tag \
         is the whole job.\n\
         - If the project already has a Google Analytics tag, replace it. Two \
         tags double every number.\n\
         - Do not send your own page_view on route changes. GA4 already counts \
         them from history events, and a second one counts every navigation \
         twice.\n\n\
         Then publish the project so the tag goes live.",
        tag = tag_snippet(measurement_id),
    )
}

fn print_next(host: &str) {
    println!(
        "  {} then {} to watch {host} arrive\n",
        dim("↳"),
        bold("craft live"),
    );
}

/// Remember the property, and open on it from now on.
fn save(id: &str, host: &str) -> Result<()> {
    let mut cfg = Config::load()?;
    cfg.upsert(id, Some(host.to_string()));
    cfg.save()
}

// ------------------------------------------------------------------ inputs ---

/// The bare host out of whatever somebody typed.
///
/// People paste what they have: a bare domain, the address bar, a link with a
/// path on it. All three name the same site, and refusing two of them would be
/// pedantry. What is refused is input that names something *else* — a path, a
/// port, an email address — because a data stream's default URI is an origin,
/// and silently discarding half of what was typed is how the wrong site gets
/// measured.
pub fn host_of(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('.');
    let without_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    // A trailing slash, or a path/query/fragment, all end the host.
    let host = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();

    if host.is_empty() {
        bail!("which domain? e.g. craft configure example.com");
    }
    if host.contains('@') {
        bail!("{host} looks like an email address, not a domain");
    }
    if host.contains(':') {
        bail!("{host} names a port; Analytics measures an origin — drop the port");
    }
    if !host.contains('.') {
        bail!("{host} is not a domain — it needs a dot, e.g. example.com");
    }
    if host
        .chars()
        .any(|c| !c.is_ascii_alphanumeric() && c != '-' && c != '.')
    {
        bail!("{host} is not a domain anacraft can read — pass it as example.com");
    }
    Ok(host)
}

/// This machine's IANA time zone, which is what a GA4 property wants.
///
/// `TZ` first, then the `/etc/localtime` symlink that both macOS and Linux
/// keep. Neither is guaranteed, so the caller handles `None` rather than this
/// inventing a plausible-looking zone — a property reporting in the wrong day
/// boundary is a subtle thing to discover later.
fn local_timezone() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim().trim_start_matches(':').to_string();
        if is_iana(&tz) {
            return Some(tz);
        }
    }
    let link = std::fs::read_link("/etc/localtime").ok()?;
    let path = link.to_string_lossy();
    let tz = path.split_once("zoneinfo/").map(|(_, tz)| tz)?.to_string();
    is_iana(&tz).then_some(tz)
}

/// Shaped like `Area/City`, which is all that can be checked without shipping
/// the tz database. Google rejects anything it does not recognise, and says so.
pub(crate) fn is_iana(tz: &str) -> bool {
    let mut parts = tz.split('/');
    let (Some(area), Some(city)) = (parts.next(), parts.next()) else {
        return false;
    };
    !area.is_empty()
        && !city.is_empty()
        && tz
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+'))
}

// ------------------------------------------------------------------ delete ---

/// Where the console keeps a property's own settings, `{id}` being the numeric
/// property id. The `p` prefix is how GA4 addresses a property in the console's
/// URL fragment.
const ADMIN_URL: &str = "https://analytics.google.com/analytics/web/#/p{id}/admin";

/// `craft delete <domain|id>` — forget a property here, and either say where
/// the console's own delete lives or, with `--all`, use it.
///
/// The default stays the harmless one. A bare `craft delete` touches nothing in
/// Google: it drops the property from this machine's config so the dashboard
/// stops opening on it, then prints the console path and what that path costs.
/// A mistyped domain there is a config edit, not somebody's analytics history.
///
/// `--all` is the answer to the obvious complaint about that — `craft
/// configure` makes a property in one line and it was strange for the way back
/// out to be four console screens. It calls `properties.delete`, which
/// `analytics.edit` has always covered and this binary has until now declined
/// to use. There is no confirmation prompt, because the flag is the
/// confirmation and a prompt that follows an explicit `--all` only teaches
/// people to hit `y`; what actually makes it safe is on Google's side, where
/// the delete is a soft one and the property waits in the account's trash for
/// 35 days.
pub async fn delete(target: &str, all: bool) -> Result<()> {
    let mut cfg = Config::load()?;
    let found = resolve(&cfg, target).await?;

    // Google first, and only then the config. Forgetting is local and
    // reversible; a failed API call is not a reason to have already pointed
    // the dashboard away from a property that is still sitting there.
    if all {
        let ga = Ga::new()?;
        let why = format!(
            "deleting {} needs permission to change your Analytics account",
            found.label(),
        );
        ga.auth()
            .ensure_scope(SCOPE_EDIT, &why)
            .await?
            .show(&crate::auth::GRANTED);
        ga.delete_property(&found.id)
            .await
            .with_context(|| format!("deleting property {}", found.id))?;
    }

    let forgotten = cfg.remove(&found.id);
    if forgotten {
        cfg.save()?;
    }

    println!("\n{}\n", panel_top("DELETE A PROPERTY"));
    // Nothing local or remote could name it, so the id is all there is to say
    // — and saying it twice reads like a bug.
    if found.name.is_empty() {
        println!(
            "  {}",
            bold(&paint(&format!("property {}", found.id), ore::diamond()))
        );
    } else {
        println!(
            "  {} {}",
            bold(&paint(&found.name, ore::diamond())),
            dim(&format!("({})", found.id)),
        );
    }

    if all {
        println!(
            "  {} {}",
            paint("✓", ore::emerald()),
            dim("moved to Google's trash — it has stopped collecting"),
        );
    }

    if forgotten {
        println!(
            "  {} {}\n",
            paint("✓", ore::emerald()),
            dim("forgotten here — craft no longer opens on it"),
        );
    } else {
        println!(
            "  {}\n",
            dim("was not configured here, so nothing to forget")
        );
    }

    if all {
        // The undo, said before it expires rather than after. 35 days is
        // generous and easy to assume is forever.
        println!(
            "  {} {}",
            paint("!", ore::redstone()),
            bold("35 days in the trash, then it and its data are gone for good"),
        );
        println!("  {}\n", dim("restore it before then from the console:"));
        println!("    {}", ADMIN_URL.replace("{id}", &found.id));
        println!("    {}\n", dim("Account column → Trash Can → Restore"));
    } else {
        // The distinction that matters. Somebody who reads only the tick above
        // would walk away believing their data was gone.
        println!(
            "  {} {}",
            paint("!", ore::redstone()),
            bold("the property and its data are still in Google"),
        );
        println!(
            "  {}\n",
            dim("nothing was deleted there — the console's delete is here:"),
        );
        println!("    {}", ADMIN_URL.replace("{id}", &found.id));
        println!("    {}", dim("Property column → Property details"));
        println!("    {}\n", dim("Move to Trash Can"));
        println!(
            "  {}",
            dim("needs Editor or above. It sits in the trash for 35 days — restorable")
        );
        println!(
            "  {}\n",
            dim("from Admin → Account → Trash — and is permanently deleted after that.")
        );
    }
    println!("{}\n", panel_bottom());

    if all {
        return Ok(());
    }

    // Both ways on from here, because the run that only forgot a property is
    // the run where somebody has not decided yet.
    println!(
        "  {} or delete it in Google too: {}",
        dim("↳"),
        bold(&format!("craft delete {target} --all")),
    );
    if forgotten {
        println!(
            "  {} changed your mind? {}",
            dim("↳"),
            bold(&format!("craft use {}", found.id)),
        );
    }
    println!();
    Ok(())
}

/// A property named by id or by the domain it measures.
struct Found {
    id: String,
    /// Empty when nothing local or remote could name it, which is not a reason
    /// to refuse — the id is what the console link needs.
    name: String,
}

impl Found {
    /// How to refer to it on the consent screen's reason line.
    fn label(&self) -> String {
        if self.name.is_empty() {
            format!("property {}", self.id)
        } else {
            self.name.clone()
        }
    }
}

/// Resolve what the user typed, preferring answers that need no network.
///
/// A numeric id is already the answer. A domain usually is too: `craft
/// configure` names the properties it creates after their host, so the local
/// config can normally answer without asking Google at all — which keeps this
/// working on a plane, and keeps a command that deletes nothing from needing a
/// live login.
async fn resolve(cfg: &Config, target: &str) -> Result<Found> {
    let raw = target.trim();
    if raw.is_empty() {
        bail!("which property? pass a domain or a numeric id");
    }

    let id = crate::config::normalize(raw);
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
        return Ok(Found {
            name: cfg
                .find(&id)
                .map(|p| p.display())
                .filter(|name| name != &format!("property {id}"))
                .unwrap_or_default(),
            id,
        });
    }

    let host = host_of(raw)?;
    if let Some(property) = cfg.properties.iter().find(|p| {
        p.name.as_deref() == Some(host.as_str()) || p.label.as_deref() == Some(host.as_str())
    }) {
        return Ok(Found {
            id: property.id.clone(),
            name: host,
        });
    }

    // Not configured here under that name, so ask which property measures it.
    let ga = Ga::new()?;
    let (existing, _) = find_existing(&ga, &host).await?;
    match existing {
        Some(Existing::Measured(property, _)) | Some(Existing::Unfinished(property)) => Ok(Found {
            id: property.id,
            name: property.name,
        }),
        None => bail!(
            "nothing here measures {host}.\n  \
             Run `craft props` for the properties this account can read, then pass \
             the id."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paid_up_machine_is_asked_for_nothing() {
        // The landing page a subscriber lands on is the plain one. A button
        // there would be an offer to pay twice.
        let landing = Paywall::Paid.landing();
        assert!(landing.cta.is_none());
        // And it has to say so out loud. This is the page that used to greet a
        // subscriber on a new laptop with "Become an Anacrafter", so silence
        // about the subscription is the bug rather than the absence of it.
        assert!(
            landing.body.contains("already an"),
            "nothing said about the subscription: {}",
            landing.body
        );
    }

    #[test]
    fn the_ask_carries_the_checkout_the_terminal_is_polling() {
        // The token on the button and the token being waited on are the same
        // one, or the payment lands somewhere nothing is watching.
        let paywall = Paywall::Owed {
            token: "tok".into(),
            checkout: "https://anacraft.dev/pricing.html?client_reference_id=tok".into(),
            note: "$2.99/month · cancel any time".into(),
            on_consent: true,
        };
        let cta = paywall.landing().cta.expect("the ask is the whole point");
        assert!(
            cta.url.contains("client_reference_id=tok"),
            "got: {}",
            cta.url
        );
        assert!(cta.note.contains("$2.99"));
    }

    #[test]
    fn a_tab_opening_out_of_nowhere_lands_on_the_plans() {
        // Nobody is dropped straight onto a card field by a command they ran
        // to create a property. Where there was no consent page to carry the
        // ask, the pricing page carries it — and the token rides along, so the
        // click after it still lands on this account.
        let url = crate::license::checkout_url(crate::PRICING_URL, "tok", Some("me@x.io"));
        assert!(url.starts_with(crate::PRICING_URL), "got: {url}");
        assert!(!url.contains("buy.stripe.com"), "got: {url}");
        assert!(url.contains("client_reference_id=tok"), "got: {url}");
        assert!(url.contains("prefilled_email=me%40x.io"), "got: {url}");
    }

    #[test]
    fn the_button_on_the_consent_page_is_the_ask_and_goes_straight_there() {
        // The other half of the same rule. This button has already said what
        // it costs, so it does not re-ask: it is the one place in the CLI that
        // opens Stripe directly, and it still carries the token.
        let url = crate::license::checkout_url(crate::SUBSCRIBE_URL, "tok", Some("me@x.io"));
        assert!(url.starts_with("https://buy.stripe.com/"), "got: {url}");
        assert!(url.contains("client_reference_id=tok"), "got: {url}");
        assert!(url.contains("prefilled_email=me%40x.io"), "got: {url}");
    }

    #[test]
    fn reads_the_host_out_of_whatever_was_typed() {
        for raw in [
            "example.com",
            "  example.com  ",
            "https://example.com",
            "http://example.com/",
            "https://example.com/pricing?ref=x#top",
            "EXAMPLE.COM",
            "example.com.",
        ] {
            assert_eq!(host_of(raw).unwrap(), "example.com", "input: {raw}");
        }
    }

    #[test]
    fn a_subdomain_is_its_own_site() {
        // www.example.com and example.com are different origins, and which one
        // the tag ends up on is the user's call, not ours to normalise away.
        assert_eq!(host_of("www.example.com").unwrap(), "www.example.com");
        assert_eq!(host_of("blog.example.co.uk").unwrap(), "blog.example.co.uk");
    }

    #[test]
    fn refuses_input_that_names_something_other_than_a_site() {
        for raw in ["", "   ", "localhost", "me@example.com", "example.com:3000"] {
            assert!(host_of(raw).is_err(), "accepted: {raw:?}");
        }
        // A path is dropped, but the host in front of it still has to be one.
        assert!(host_of("https:///pricing").is_err());
    }

    #[test]
    fn the_measurement_id_lands_in_both_places_the_tag_needs_it() {
        // The classic broken install is one of the two filled in and the other
        // left as a placeholder. The tag loads, nothing is recorded, and GA4
        // reports only that no data has arrived.
        let snippet = tag("G-1A2BCD345E");
        assert_eq!(
            snippet.matches("G-1A2BCD345E").count(),
            2,
            "the id belongs in the script src and the config call:\n{snippet}"
        );
        assert!(snippet.contains(
            "<script async src=\"https://www.googletagmanager.com/gtag/js?id=G-1A2BCD345E\">"
        ));
        assert!(snippet.contains("gtag('config', 'G-1A2BCD345E');"));
        // No placeholder survived into what somebody is about to paste.
        assert!(!snippet.contains("G-XXXXXXXXXX"));
    }

    #[test]
    fn the_tag_is_the_one_the_setup_guide_documents() {
        // docs/setup-ga4.html step 03 prints this snippet for people doing it
        // by hand. The two must not drift: a difference here would read as one
        // of them being wrong.
        let snippet = tag("G-1A2BCD345E");
        for line in [
            "<!-- Google tag (gtag.js) -->",
            "window.dataLayer = window.dataLayer || [];",
            "function gtag(){dataLayer.push(arguments);}",
            "gtag('js', new Date());",
            "</script>",
        ] {
            assert!(snippet.contains(line), "missing {line:?}:\n{snippet}");
        }
    }

    #[test]
    fn the_bare_snippet_is_paste_safe_and_carries_the_id_both_places() {
        // What the MCP `configure_site` tool hands over in JSON: same id in
        // the same two places, and no ANSI or panel belong to a string another
        // program is going to read.
        let snippet = tag_snippet("G-1A2BCD345E");
        assert_eq!(
            snippet.matches("G-1A2BCD345E").count(),
            2,
            "the id belongs in the script src and the config call:\n{snippet}"
        );
        assert!(
            !snippet.contains('\u{1b}'),
            "escape codes leaked into the plain snippet:\n{snippet}"
        );
        assert!(snippet.contains(
            "<script async src=\"https://www.googletagmanager.com/gtag/js?id=G-1A2BCD345E\">"
        ));
        assert!(snippet.contains("gtag('config', 'G-1A2BCD345E');"));
    }

    #[tokio::test]
    async fn a_numeric_id_resolves_without_asking_google() {
        let cfg = Config {
            properties: vec![crate::config::Property {
                id: "397412345".into(),
                name: Some("example.com".into()),
                ..Default::default()
            }],
            ..Config::default()
        };

        for typed in ["397412345", " properties/397412345 "] {
            let found = resolve(&cfg, typed).await.unwrap();
            assert_eq!(found.id, "397412345");
            assert_eq!(found.name, "example.com");
        }
    }

    #[tokio::test]
    async fn an_id_that_is_not_configured_still_resolves_to_itself() {
        // The console link only needs the id, so an unknown one is not a
        // reason to refuse — `craft delete` is how somebody cleans up a
        // property this machine never had.
        let found = resolve(&Config::default(), "397412345").await.unwrap();
        assert_eq!(found.id, "397412345");
        assert!(
            found.name.is_empty(),
            "invented a name for a property it knows nothing about: {}",
            found.name
        );
    }

    #[tokio::test]
    async fn a_domain_resolves_from_the_local_config_before_the_network() {
        // `craft configure` names what it creates after the host, so the
        // common case answers offline. Reaching the network here would make a
        // command that deletes nothing require a live login.
        let cfg = Config {
            properties: vec![
                crate::config::Property {
                    id: "111".into(),
                    name: Some("other.com".into()),
                    ..Default::default()
                },
                crate::config::Property {
                    id: "222".into(),
                    name: Some("example.com".into()),
                    ..Default::default()
                },
            ],
            ..Config::default()
        };

        let found = resolve(&cfg, "https://example.com/pricing").await.unwrap();
        assert_eq!(found.id, "222");
        assert_eq!(found.name, "example.com");
    }

    #[tokio::test]
    async fn a_domain_matches_a_nickname_too() {
        let cfg = Config {
            properties: vec![crate::config::Property {
                id: "333".into(),
                name: Some("Marketing site".into()),
                label: Some("example.com".into()),
                ..Default::default()
            }],
            ..Config::default()
        };
        assert_eq!(resolve(&cfg, "example.com").await.unwrap().id, "333");
    }

    #[tokio::test]
    async fn an_empty_target_is_a_question_not_a_lookup() {
        assert!(resolve(&Config::default(), "   ").await.is_err());
    }

    #[test]
    fn a_time_zone_has_to_look_like_one() {
        assert!(is_iana("America/Los_Angeles"));
        assert!(is_iana("Asia/Dhaka"));
        assert!(is_iana("America/Argentina/Buenos_Aires"));
        for bad in ["UTC", "", "/", "PST8PDT", "Europe/", "/London", "a b/c"] {
            assert!(!is_iana(bad), "accepted: {bad:?}");
        }
    }
}
