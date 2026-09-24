//! anacraft — Google Analytics 4 in your terminal, wearing a texture pack.

mod achievements;
mod audit;
mod auth;
mod avatar;
mod burn;
mod config;
mod configure;
mod ga;
mod license;
mod mcp;
mod render;
mod report;
mod serve;
mod slack;
mod theme;
mod ui;
mod watch;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use config::Config;
use ga::{DateRange, Ga, ReportRequest};
use render::{bold, dim, paint, panel_bottom, panel_top};
use report::{Format, Overview};
use theme::{ore, OVERVIEW};

const DEFAULT_DAYS: u32 = 7;
const DEFAULT_LIMIT: i32 = 10;
const TOP_PAGES: &str = "TOP PAGES";
const TOP_PORTALS: &str = "TOP PORTALS";
const TOP_REALMS: &str = "TOP REALMS";

#[derive(Parser)]
#[command(
    name = "craft",
    version,
    about = "Google Analytics in your terminal — set it up, audit it, read it",
    long_about = None,
    after_help = "Run `craft` with no command to open the live dashboard.\n\
                  With no property saved it runs on synthetic data, so it works \
                  before you sign in."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// GA4 property id to query, overriding the saved default.
    #[arg(long, short, global = true)]
    property: Option<String>,

    /// Palette to render with, overriding the saved default.
    #[arg(long, short, global = true)]
    theme: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in with Google and store a refresh token.
    Login,
    /// Revoke and delete the stored credentials.
    Logout,
    /// List every GA4 property this account can read.
    Props,
    /// Set the default property.
    Use {
        /// Numeric property id, e.g. 397412345
        id: String,
    },
    /// Forget a property here, or with --all delete it in Google too.
    ///
    /// On its own this changes nothing in Analytics: it forgets the property
    /// you name, so the dashboard stops opening on it, then prints the console
    /// link and the two clicks that do the deletion.
    ///
    /// Pass --all to do those two clicks for you. The property goes to your
    /// Analytics account's trash, where Google keeps it restorable for 35 days
    /// before deleting it and its data for good.
    Delete {
        /// The property to let go of: a domain, or a numeric property id.
        target: String,
        /// Delete it in Google too, not just here. Moves the property to the
        /// Analytics trash — restorable from the console for 35 days.
        #[arg(long, visible_alias = "remove")]
        all: bool,
    },
    /// Set a website up in GA4 and print the tag to paste on it.
    ///
    /// Creates the property and the web data stream for a domain, then prints
    /// the gtag.js snippet with its measurement id already filled in — the
    /// first three steps of the setup guide, without the console. Run it twice
    /// and it reuses what it made the first time, so it is also how to get the
    /// tag back.
    ///
    /// This is the one command that changes anything in Analytics, so it asks
    /// Google for permission to do so when you run it, and not before.
    ///
    /// Part of the Anacrafter subscription. The ask arrives on the page the
    /// sign-in already ends on, and the terminal picks the payment up from
    /// there without anything to paste.
    Configure {
        /// The site to measure, e.g. example.com
        domain: String,
        /// Analytics account to create the property under. Only needed when
        /// the signed-in account can see more than one.
        #[arg(long)]
        account: Option<String>,
        /// IANA reporting time zone, e.g. America/New_York. Defaults to this
        /// machine's.
        #[arg(long)]
        timezone: Option<String>,
        /// ISO 4217 reporting currency.
        #[arg(long, default_value = "USD")]
        currency: String,
    },
    /// Headline metrics for the period, with deltas and achievements.
    Overview {
        /// Days to look back, ending yesterday.
        #[arg(long, short, default_value_t = DEFAULT_DAYS)]
        days: u32,
        /// How to render: panels for a terminal, json for a script, slack for
        /// a webhook.
        #[arg(long, short, value_enum, default_value = "panels")]
        format: Format,
    },
    /// Most-visited pages.
    Pages {
        #[arg(long, short, default_value_t = DEFAULT_DAYS)]
        days: u32,
        #[arg(long, short, default_value_t = DEFAULT_LIMIT)]
        limit: i32,
    },
    /// Where traffic arrives from.
    Portals {
        #[arg(long, short, default_value_t = DEFAULT_DAYS)]
        days: u32,
        #[arg(long, short, default_value_t = DEFAULT_LIMIT)]
        limit: i32,
    },
    /// Traffic by country.
    Realms {
        #[arg(long, short, default_value_t = DEFAULT_DAYS)]
        days: u32,
        #[arg(long, short, default_value_t = DEFAULT_LIMIT)]
        limit: i32,
    },
    /// Who is on the site right now.
    Live,
    /// Render a dashboard from synthetic data — no Google account needed.
    Demo,
    /// List the palettes, or save one as the default.
    Theme {
        /// Palette name. Omit to list what is available.
        name: Option<String>,
    },
    /// Full-screen live dashboard. Runs when no command is given.
    Dash {
        /// Days to look back. Defaults to the property's setting, else 7.
        #[arg(long, short)]
        days: Option<u32>,
        /// Seconds between refreshes. Defaults to the property's setting, else 30.
        #[arg(long)]
        refresh: Option<u64>,
        /// Seconds between realtime polls — the htop-style tick. Minimum 2.
        #[arg(long)]
        live_refresh: Option<u64>,
        /// Drive the dashboard from synthetic data — no Google account needed.
        #[arg(long)]
        demo: bool,
    },
    /// Check how the property is measuring, and report what is broken.
    ///
    /// The other commands answer "what happened". This one answers the
    /// question before it: is this property measuring the site at all, and is
    /// what it measured worth trusting. Fifteen checks over four weeks —
    /// whether anything is marked as a key event and whether those events ever
    /// fire, whether outcomes are arriving unmarked, whether the stream is
    /// measuring what it was told to and recording what it measures, whether
    /// purchases carry their revenue, whether page views are being counted
    /// twice, whether the site or a payment page is referring itself, whether
    /// an event stopped firing between releases.
    ///
    /// Read-only unless `--fix` is passed, and then only for the settings it
    /// printed first. Exits 2 when it finds something it did not fix, so a
    /// script can tell.
    Audit {
        /// Days to look back, ending yesterday. The previous window of the
        /// same length is what "stopped firing" is measured against.
        #[arg(long, short, default_value_t = audit::DEFAULT_DAYS)]
        days: u32,
        /// How to render: panels for a person, json for a script, slack for a
        /// webhook payload.
        #[arg(long, short, value_enum, default_value = "panels")]
        format: Format,
        /// Apply the fixes the report offers. Only ever configuration the
        /// property already implies — marking outcomes that are arriving,
        /// turning on measurement the tag already supports — and every one of
        /// them is printed before it is made and undone from the GA4 console.
        /// Needs Editor on the property.
        #[arg(long)]
        fix: bool,
        /// Audit synthetic data — no Google account, no subscription.
        #[arg(long)]
        demo: bool,
    },
    /// Check the numbers against their own recent normal, and report what moved.
    ///
    /// Compares the most recent complete day against the mean of the days
    /// before it. Needs no configuration to be useful — a site's own history
    /// is the threshold — and takes per-metric percentages under
    /// `[property.watch]` for anything that should be tighter or quieter.
    /// Exits 2 when something fired, so a script can tell.
    Watch {
        /// Days of history the baseline averages over.
        #[arg(long)]
        baseline: Option<u32>,
        /// Keep checking every N seconds instead of checking once and exiting.
        #[arg(long, value_name = "SECONDS")]
        every: Option<u64>,
        /// POST the alert to a Slack incoming webhook. Also read from
        /// ANACRAFT_WEBHOOK, and deliberately not from config.toml — that
        /// file is meant to be safe to commit, and this URL is not.
        #[arg(long)]
        webhook: Option<String>,
        /// How to render: panels for a person, json for a script, slack for a
        /// webhook payload.
        #[arg(long, short, value_enum, default_value = "panels")]
        format: Format,
        /// Alert on synthetic data — no Google account, no subscription.
        #[arg(long)]
        demo: bool,
    },
    /// Install anacraft into a Slack workspace, so alerts have somewhere to go.
    ///
    /// With no flag it says where alerts currently go. `--install` opens
    /// Slack's own install screen, where the workspace and channel are picked,
    /// and saves the webhook it hands back to `~/.anacraft/slack.json` —
    /// `craft watch` then needs no `--webhook` at all.
    Slack {
        /// Run the install: opens the browser, then saves what Slack returns.
        #[arg(long)]
        install: bool,
        /// Forget the saved webhook. The app stays installed in Slack.
        #[arg(long, conflicts_with = "install")]
        uninstall: bool,
        /// Post one message, to check the install before an alert needs it.
        #[arg(long, conflicts_with_all = ["install", "uninstall"])]
        test: bool,
    },
    /// Serve the dashboard's numbers to an AI assistant over MCP.
    ///
    /// Speaks the Model Context Protocol on stdin/stdout, so an MCP client
    /// spawns it as a child process. Read-only, and part of the subscription.
    Mcp {
        /// Serve synthetic data — no Google account, no subscription.
        #[arg(long)]
        demo: bool,
        /// Install into Claude Desktop and Smartloop instead of serving.
        /// Pair with --demo to install synthetic-data servers.
        #[arg(long, conflicts_with = "uninstall")]
        install: bool,
        /// Take the server back out of Claude Desktop's config, leaving any
        /// other servers in there alone.
        #[arg(long, conflicts_with = "install")]
        uninstall: bool,
    },
    /// Run the register-a-tag flow as an API on this machine.
    ///
    /// Opens a page in your browser: sign in with Google, pick or create a
    /// property, copy the tag. Every endpoint the page uses is documented at
    /// https://anacraft.dev/serve.html, so a script can do the same.
    Serve {
        /// Port to listen on. The OS picks one by default.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Print the URL instead of opening a browser.
        #[arg(long)]
        no_open: bool,
        /// Use this bearer token instead of minting one.
        #[arg(long)]
        token: Option<String>,
        /// Stop after this many idle minutes. 0 runs until Ctrl-C.
        #[arg(long, default_value_t = 60)]
        idle: u64,
        /// Answer everything synthetically — no Google account, no
        /// subscription, and nothing created in any Analytics account.
        #[arg(long)]
        demo: bool,
        /// Address to listen on. Anything but loopback needs --public-url.
        #[arg(long, default_value = "127.0.0.1")]
        host: std::net::IpAddr,
        /// Serve the pages to anybody at this https origin, each visitor
        /// signing in on a session of their own — how app.anacraft.dev runs.
        /// Needs ANACRAFT_WEB_OAUTH_CLIENT_ID and _SECRET (a Web client)
        /// unless --demo.
        #[arg(long, conflicts_with = "token")]
        public_url: Option<String>,
    },
    /// Start an Anacraft subscription, or pick up the one you have.
    ///
    /// Opens Stripe, waits for the payment to clear, and writes the plan into
    /// the config itself. The subscription is keyed to the Google account you
    /// signed in with, so a second machine only has to sign in.
    Subscribe {
        /// Which plan to take — `basic` ($2.99, the Anacrafter),
        /// `pro` ($5.99, adds Slack alerts) or `elite` ($9.99, adds
        /// `craft mcp`). With no plan named, it opens the pricing page, where
        /// the three stand side by side.
        #[arg(long, value_enum)]
        plan: Option<license::Tier>,
        /// Only look up where the account already stands. Opens no browser,
        /// so it is the one to run on a second machine or from a script.
        #[arg(long)]
        check: bool,
    },
    /// Mint the badge for your own site — how many other sites link to it.
    ///
    /// Counts distinct referring domains over the last thirty days and prints
    /// the HTML to paste. Run once: the number is served rather than baked in,
    /// and the dashboard, `craft watch` and the MCP server recount and
    /// republish it as they run, so the badge on the page keeps up without
    /// this command being run again. Free, and deliberately: a badge exists to
    /// be seen by people who have never heard of anacraft.
    Burn {
        /// Which palette to render it in. Keeps the badge's own by default.
        #[arg(long)]
        theme: Option<String>,
        /// The words beside the number. Default: "sites link here".
        #[arg(long)]
        label: Option<String>,
    },
    /// Print the site's dashboard captures as HTML. Used by `make capture`.
    #[command(hide = true)]
    Capture,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!(
            "\n  {} {}\n",
            paint("⛏ ", ore::redstone()),
            bold(&err.to_string())
        );
        // Surface the cause chain, which is where API detail usually lives.
        for cause in err.chain().skip(1) {
            eprintln!("     {}", dim(&cause.to_string()));
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load()?;

    // Resolve the palette before anything renders: the flag wins, then the
    // saved default. An unknown name is worth saying out loud rather than
    // silently falling back.
    // A property may carry its own palette, so resolve which property we are
    // on before picking one. No property configured yet is not an error here —
    // `demo` and `login` both run fine without one.
    let active_id = cfg.resolve_property(cli.property.as_deref()).ok();
    let palette = cli.theme.as_deref().or(match active_id.as_deref() {
        Some(id) => cfg.theme_for(id),
        None => cfg.theme.as_deref(),
    });
    if let Some(name) = palette {
        if !theme::select(name) {
            anyhow::bail!("no theme called {name} — run `craft theme` to list them");
        }
    }

    match cli.command.unwrap_or(Command::Dash {
        days: None,
        refresh: None,
        live_refresh: None,
        demo: cfg.active_property().is_none(),
    }) {
        Command::Demo => cmd_demo(),
        Command::Capture => {
            print!("{}", ui::capture()?);
            Ok(())
        }
        Command::Theme { name } => cmd_theme(name.as_deref()),
        Command::Burn { theme, label } => {
            // `--theme` is also a global, so a name typed after `burn` can
            // land in either field. Both mean the same thing here — restyle
            // the badge — and reading only one of them would make the flag
            // work or not depending on where clap put it.
            burn::run(
                &cfg.resolve_property(cli.property.as_deref())?,
                theme.or_else(|| cli.theme.clone()),
                label,
            )
            .await
        }
        Command::Subscribe { plan, check } => cmd_subscribe(plan, check).await,
        Command::Serve {
            port,
            no_open,
            token,
            idle,
            demo,
            host,
            public_url,
        } => {
            serve::run(serve::Options {
                port,
                open: !no_open,
                token,
                idle,
                demo,
                property: cli.property.clone(),
                host,
                public_url,
            })
            .await
        }
        Command::Mcp {
            demo,
            install,
            uninstall,
        } => {
            if uninstall {
                mcp::uninstall()
            } else if install {
                mcp::install_all(demo)
            } else {
                mcp::serve(demo, cli.property.as_deref()).await
            }
        }
        Command::Audit {
            days,
            format,
            fix,
            demo,
        } => {
            audit::run(
                &cfg,
                cli.property.as_deref(),
                audit::Options {
                    days,
                    format,
                    demo,
                    fix,
                },
            )
            .await
        }
        Command::Watch {
            baseline,
            every,
            webhook,
            format,
            demo,
        } => {
            // The flag wins, then the environment, then whatever
            // `craft slack --install` saved. Never the config file: a URL that
            // posts into somebody's Slack does not belong in a file the README
            // calls safe to commit to a dotfile repo.
            let webhook = webhook
                .or_else(|| {
                    std::env::var("ANACRAFT_WEBHOOK")
                        .ok()
                        .filter(|url| !url.trim().is_empty())
                })
                .or_else(|| slack::Install::load().map(|saved| saved.webhook_url));
            watch::run(
                &cfg,
                cli.property.as_deref(),
                watch::Options {
                    baseline,
                    every,
                    webhook,
                    format,
                    demo,
                },
            )
            .await
        }
        Command::Slack {
            install,
            uninstall,
            test,
        } => {
            if install {
                slack::install().await
            } else if uninstall {
                slack::uninstall()
            } else if test {
                slack::test().await
            } else {
                slack::status()
            }
        }
        Command::Login => cmd_login().await,
        Command::Logout => cmd_logout().await,
        Command::Props => cmd_props().await,
        Command::Use { id } => cmd_use(&id).await,
        Command::Delete { target, all } => configure::delete(&target, all).await,
        Command::Configure {
            domain,
            account,
            timezone,
            currency,
        } => {
            configure::run(
                &domain,
                configure::Options {
                    account,
                    timezone,
                    currency,
                },
            )
            .await
        }
        Command::Overview { days, format } => {
            cmd_overview(
                &cfg.resolve_property(cli.property.as_deref())?,
                days,
                format,
            )
            .await
        }
        Command::Pages { days, limit } => {
            let property = cfg.resolve_property(cli.property.as_deref())?;
            cmd_ranked(
                &property,
                days,
                limit,
                "pagePath",
                "screenPageViews",
                "views",
                TOP_PAGES,
            )
            .await
        }
        Command::Portals { days, limit } => {
            let property = cfg.resolve_property(cli.property.as_deref())?;
            cmd_ranked(
                &property,
                days,
                limit,
                "sessionSourceMedium",
                "sessions",
                "sessions",
                TOP_PORTALS,
            )
            .await
        }
        Command::Realms { days, limit } => {
            let property = cfg.resolve_property(cli.property.as_deref())?;
            cmd_ranked(
                &property,
                days,
                limit,
                "country",
                "totalUsers",
                "users",
                TOP_REALMS,
            )
            .await
        }
        Command::Live => cmd_live(&cfg.resolve_property(cli.property.as_deref())?).await,
        Command::Dash {
            days,
            refresh,
            live_refresh,
            demo,
        } => {
            if demo {
                // Checked before resolving a property, so the demo works on a
                // machine that has never logged in. The report cadence is
                // pinned short so the synthetic numbers visibly move.
                return ui::run_demo(
                    days.unwrap_or(7),
                    refresh.unwrap_or(30).min(5),
                    live_refresh.unwrap_or(ui::LIVE_EVERY),
                )
                .await;
            }
            let property = cfg.resolve_property(cli.property.as_deref())?;
            // Ask Supabase where the subscription stands on the way in. It is
            // cached, short-timeout and best-effort — the star it decides is
            // never worth making somebody wait for their numbers.
            let tier = license::sync(&cfg).await;
            let cfg = match tier {
                Some(tier) if cfg.tier() != Some(tier) => Config::load()?,
                None if cfg.tier().is_some() => Config::load()?,
                _ => cfg,
            };
            // Flags win; otherwise fall back to what this property saved.
            let saved = cfg.find(&property);
            let settings = ui::Settings {
                days: days.or_else(|| saved.and_then(|p| p.days)).unwrap_or(7),
                refresh: refresh
                    .or_else(|| saved.and_then(|p| p.refresh))
                    .unwrap_or(30),
                live_refresh: live_refresh
                    .or_else(|| saved.and_then(|p| p.live_refresh))
                    .unwrap_or(ui::LIVE_EVERY),
            };
            // The badge, if this machine minted one for the property being
            // opened: recounted and republished from here for as long as the
            // dashboard is up. Spawned, silent and skipped when there is no
            // badge — see `burn::keep_current`.
            burn::keep_current(&property);
            ui::run(&cfg, &property, settings).await
        }
    }
}

// ---------------------------------------------------------------- accounts ---

/// Where the CLI sends somebody who wants to subscribe: the site's pricing
/// page, not the Payment Link behind its Subscribe button.
///
/// A tab that opens straight onto a card field asks for a decision before
/// showing what it is for. The page is where the plans, the table of which
/// command is on which side, and the sentence about paying with the same email
/// as the Google account all are — and it is one click from the same checkout,
/// so nothing is further away than a click that was worth being asked for.
///
/// The token still travels. `license::checkout_url` hangs
/// `client_reference_id` and `prefilled_email` on this URL and the page's own
/// script forwards both onto its Subscribe button, so a checkout begun in the
/// terminal still lands on the account the moment it clears. If it ever did
/// not, the payment is adopted by email on the next lookup instead — later,
/// not lost.
///
/// The Payment Link itself now lives in exactly one place, `docs/pricing.html`,
/// which is one fewer copy to keep in step when it changes.
///
/// `pub(crate)` because `craft configure` sends people to the same page — from
/// the page its OAuth trip already ends on, rather than through a second tab.
pub(crate) const PRICING_URL: &str = "https://anacraft.dev/pricing.html";

/// Stripe's hosted page for the Basic plan — the Payment Link the pricing
/// page's first Subscribe button points at, and the one place in the CLI that
/// goes straight there.
///
/// That place is the button on the page Google's consent screen hands back
/// (see `configure::Paywall`): `craft configure` is Basic, so its ask opens
/// the plan's own checkout. Everywhere else opens [`PRICING_URL`] instead, and
/// the difference is whether the ask has been made yet. A tab that opens out of
/// nowhere onto a card field is a decision nobody was asked for. That button is
/// the asking: it is captioned "Become an Anacrafter", the price and "cancel
/// any time" are on the line beneath it, and pressing it is the answer — so
/// putting a page of plans in front of it re-asks a question that was just
/// answered.
///
/// A Payment Link rather than a checkout session built here: a session needs a
/// secret key, and a key shipped inside a binary anybody can download is a key
/// that has leaked. The link is public by design and safe to hardcode — but it
/// exists in two places, here and on `docs/pricing.html`, so a new one has to
/// land in both.
pub(crate) const SUBSCRIBE_URL: &str = "https://buy.stripe.com/3cIdR93sU4SbfECab79MY02";

/// The same two, for Pro and Elite — empty until those Payment Links exist.
///
/// A hardcoded link that 404s is worse than a plan that admits it is not ready,
/// because the first one looks like it worked. So `craft subscribe --plan pro`
/// (and `--plan elite`) checks its link first, and refuses with "not ready yet"
/// until the real URL lands here — at which point `--plan` quotes the price and
/// opens the checkout just like Basic does.
const SUBSCRIBE_PRO_URL: &str = "https://buy.stripe.com/cNieVd1kMacvgIGab79MY03";
const SUBSCRIBE_ELITE_URL: &str = "https://buy.stripe.com/3cI7sL4wY98r8caern9MY04";

/// The Payment Link a plan's `craft subscribe --plan <name>` opens.
///
/// Empty means the plan is not for sale yet: the caller that puts these in
/// front of a human checks first. Basic's is never empty — it is the plan the
/// site has sold since the start.
pub(crate) fn subscribe_url(plan: license::Tier) -> &'static str {
    match plan {
        license::Tier::Basic => SUBSCRIBE_URL,
        license::Tier::Pro => SUBSCRIBE_PRO_URL,
        license::Tier::Elite => SUBSCRIBE_ELITE_URL,
    }
}

/// The price the dashboard's ask and `craft subscribe` quote when no plan is
/// named — the entry plan, which is what a first ask is asking for.
pub(crate) fn price_line() -> &'static str {
    license::Tier::Basic.monthly()
}

/// How long to wait on a checkout before handing back a way to finish later,
/// and how often to ask. Stripe's webhook reaches Supabase within seconds of a
/// payment; the rest of the window is somebody hunting for their card.
///
/// Five minutes rather than fifteen. Stripe imposes no deadline worth matching
/// — the session a Payment Link opens is good for a day — so this is sized to
/// the person, not the service: card in hand is under a minute, and the case
/// that runs long is a 3-D Secure step-up waiting on a bank app, which is the
/// two-to-five minutes this has to cover. Fifteen made every ordinary payment
/// sit inside a window sized for the worst one, and a terminal spinning for a
/// quarter of an hour reads as something wrong rather than something waiting.
///
/// Giving up early costs almost nothing either way: the row is claimed before
/// the browser opens, the webhook fills it in whenever the payment lands, and
/// `craft subscribe --check` collects it afterwards.
const CHECKOUT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const CHECKOUT_POLL: std::time::Duration = std::time::Duration::from_secs(3);

/// Start a subscription, or pick up one that already exists.
///
/// Order matters here: ask where the account stands *before* opening anything,
/// because sending an existing subscriber to a checkout buys them a second
/// subscription. The lookup below registers the email first, so a payment made
/// on the pricing page — by them, in a browser, minutes ago — is found rather
/// than asked for twice. That same first step is the whole of `--check`, and it is what
/// makes a new laptop work — the record is keyed to the Google account, so
/// signing in is all a second machine has to do.
async fn cmd_subscribe(plan: Option<license::Tier>, check: bool) -> Result<()> {
    let account = auth::Auth::account()?;
    let record = license::Record::load();

    // Register the account before asking about it, so a payment made on the
    // pricing page — which reaches Supabase with no account attached to it — is
    // adopted by the email it was paid with. `craft login` does this too; doing
    // it here as well is what lets somebody already signed in pay in a browser
    // and pick it up with `craft subscribe --check`, rather than having to sign
    // in again to trigger the adoption. Best-effort either way: a failure here
    // only means the lookup below finds what it would have found anyway.
    if let (Some(account), Some(_)) = (&account, license::project()) {
        let _ = license::link(account).await;
    }

    // Only ever a token this account minted. The saved one outlives the
    // sign-in that wrote it, so on a machine a different account has signed
    // in on since, it is somebody else's — and asking the lookup about it
    // would answer about their subscription and then write that answer down
    // here as this account's. See `license::Record::speaks_for`.
    let token = record
        .token
        .as_deref()
        .filter(|_| record.speaks_for(account.as_ref()));

    // Nothing to ask when this build has no service, or when there is neither
    // an account nor a past checkout to ask about.
    let known = if license::project().is_some() && (account.is_some() || token.is_some()) {
        match license::fetch(account.as_ref(), token).await {
            Ok(status) => {
                // Remember every answer, not just the good ones: a cancellation
                // that is not written down is a cancellation the next launch
                // reads as a subscription.
                record.confirm(account.as_ref(), &status)?;
                Some(status)
            }
            Err(err) if check => return Err(err),
            Err(err) => {
                // The lookup being down is no reason to stand between somebody
                // and paying; carry on to checkout.
                println!("\n  {}", dim(&format!("could not reach the check: {err}")));
                None
            }
        }
    } else {
        None
    };

    if let Some(status) = &known {
        if status.is_active() && should_skip_active_checkout(status, plan) {
            return activated(status);
        }
    }

    if check {
        return match known {
            // Cancelled or lapsed: stop the flag and plan claiming otherwise.
            Some(status) if !status.is_pending() => {
                let cleared = license::set_plan(None)?;
                println!(
                    "\n  {} subscription {}{}\n",
                    paint("○", ore::stone()),
                    bold(status.label()),
                    if cleared {
                        format!(" — cleared {}", bold("supporter"))
                    } else {
                        String::new()
                    },
                );
                println!("  {} to start a new one\n", bold("craft subscribe"));
                Ok(())
            }
            _ => {
                // Names the address it asked about, because the commonest
                // reason a subscriber reads this line is that the payment is
                // sitting under a different one — a personal address typed
                // into Stripe, a work account signed into Google. An opaque
                // "nothing recorded" leaves them staring at a service that is
                // wrong; naming the address turns it into something they can
                // see at a glance and act on.
                match account.as_ref().and_then(|a| a.email.as_deref()) {
                    Some(email) => println!(
                        "\n  {} nothing recorded for {} — {} to start\n",
                        paint("○", ore::stone()),
                        bold(email),
                        bold("craft subscribe"),
                    ),
                    None => println!(
                        "\n  {} nothing recorded for this account — {} to start\n",
                        paint("○", ore::stone()),
                        bold("craft subscribe"),
                    ),
                }
                if let Some(email) = account.as_ref().and_then(|a| a.email.as_deref()) {
                    println!(
                        "  {}\n",
                        dim(&format!(
                            "already paid, but with another address? that payment is \
                             attached to the address Stripe collected, not to {email} — \
                             mail support@anacraft.dev with both and we will move it across"
                        ))
                    );
                }
                // The one case where "nothing recorded" is probably wrong: an
                // existing subscriber whose credentials predate the identity
                // scopes, so the lookup has no account to search on.
                if account.is_none() && auth::Tokens::load().ok().flatten().is_some() {
                    println!(
                        "  {}\n",
                        dim("already subscribed? run craft login again — this \
                             machine signed in before subscriptions existed")
                    );
                }
                Ok(())
            }
        };
    }

    // A build with no lookup has only the flag to go on, and sending an
    // existing subscriber back to a checkout buys them a second subscription.
    if license::project().is_none() && Config::load()?.supporter {
        println!(
            "\n  {} {}  ·  {}\n",
            paint(theme::glyph::STAR, ore::gold()),
            bold(&paint("already an Anacrafter", ore::gold())),
            dim(license::supporter_line())
        );
        return Ok(());
    }

    // A plan named means a specific checkout. A Payment Link that does not
    // exist yet is worse than a plan that says so, so `--plan pro` and `--plan
    // elite` admit they are not for sale until a real URL lands in
    // SUBSCRIBE_PRO_URL / SUBSCRIBE_ELITE_URL. Basic has been sold since the
    // start, and is never refused.
    let (url, price) = match plan {
        Some(pick) if subscribe_url(pick).is_empty() => {
            println!(
                "\n  {} the {} plan is not for sale yet — {} is {}\n",
                paint(theme::glyph::STAR, ore::gold()),
                bold(pick.label()),
                bold("craft subscribe"),
                price_line(),
            );
            return Ok(());
        }
        // A plan named opens that plan's own checkout — the person has already
        // chosen, so the pricing page between them and the card field asks a
        // question that just got answered.
        Some(pick) => (subscribe_url(pick), pick.monthly()),
        // No plan: the page, where the three stand side by side and the first
        // click does the choosing.
        None => (PRICING_URL, price_line()),
    };

    // A fresh token per checkout: an old one belongs to the old subscription,
    // which is the wrong row for somebody resubscribing after a cancellation.
    let token = license::mint_token();
    let email = account.as_ref().and_then(|a| a.email.clone());
    let checkout = license::checkout_url(url, &token, email.as_deref());

    // Claim the row before the browser opens, so the webhook has something to
    // fill in the moment the payment lands. A claim that will not go through is
    // not worth blocking a payment over: the webhook creates the row from the
    // token either way, and only the tie to the Google account is lost.
    if let (Some(account), Some(_)) = (&account, license::project()) {
        if license::claim(&token, account).await.is_err() {
            println!(
                "  {}\n",
                dim("could not record the checkout — it will still be picked up by token")
            );
        }
    }

    println!(
        "\n  {} {}  ·  {}\n",
        paint(theme::glyph::STAR, ore::gold()),
        bold(&checkout),
        price
    );
    let _ = open::that(&checkout);

    if account.is_none() {
        // Two different people to talk to: somebody who never signed in, and
        // somebody who signed in on a build that never asked who they were.
        // The second one is already logged in, so "not signed in" would read as
        // a bug rather than as an instruction.
        let known = auth::Tokens::load().ok().flatten().is_some();
        println!(
            "  {}\n",
            dim(if known {
                "signed in before subscriptions existed — run craft login again \
                 so this follows you to other machines"
            } else {
                "not signed in — run craft login afterwards so this follows you \
                 to other machines"
            })
        );
    }

    if license::project().is_none() {
        // Nothing to poll. The flag is a line of TOML and always was;
        // pretending otherwise would strand somebody who has paid.
        println!(
            "  once it's active, set {} in {}\n",
            bold("supporter = true"),
            config::Config::path()?.display()
        );
        return Ok(());
    }

    license::Record {
        token: Some(token.clone()),
        user_id: account.as_ref().map(|a| a.sub.clone()),
        ..license::Record::default()
    }
    .save()?;

    wait_for_payment(account.as_ref(), &token, "craft subscribe --check")
        .await
        .map(|_| ())
}

/// Poll Supabase until the webhook says the payment landed.
///
/// Returns whether it did, because the caller may have something waiting on
/// the answer: `craft configure` has a property to create once the payment is
/// in, and nothing to do at all if it never arrives. `next` is the command to
/// hand back when the window closes first — the way to pick this up later,
/// which is `--check` for a bare `craft subscribe` and the same configure run
/// for the one that was interrupted by it.
pub(crate) async fn wait_for_payment(
    account: Option<&auth::Account>,
    token: &str,
    next: &str,
) -> Result<bool> {
    println!(
        "  {} waiting for Stripe — {}\n",
        paint(theme::glyph::PICKAXE, ore::iron()),
        dim("^C to stop, nothing is lost")
    );

    let deadline = std::time::Instant::now() + CHECKOUT_TIMEOUT;
    let mut frame = 0usize;
    loop {
        if let Ok(status) = license::fetch(account, Some(token)).await {
            if status.is_active() {
                clear_line();
                license::Record {
                    token: Some(token.to_string()),
                    user_id: account.map(|a| a.sub.clone()),
                    status: status.clone(),
                    checked: Some(chrono::Utc::now()),
                }
                .save()?;
                activated(&status)?;
                return Ok(true);
            }
        }

        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            clear_line();
            println!(
                "\n  {} once the payment clears, run {}\n",
                paint("○", ore::stone()),
                bold(next)
            );
            return Ok(false);
        }

        spin(frame, left);
        frame += 1;
        tokio::time::sleep(CHECKOUT_POLL).await;
    }
}

/// One frame of the same spinner the dashboard uses, redrawn in place.
fn spin(frame: usize, left: std::time::Duration) {
    use std::io::Write;
    let glyph = theme::glyph::SPINNER[frame % theme::glyph::SPINNER.len()];
    print!(
        "\r  {} {}",
        paint(&glyph.to_string(), theme::accent()),
        dim(&format!("waiting · {}m left ", left.as_secs() / 60 + 1)),
    );
    let _ = std::io::stdout().flush();
}

fn clear_line() {
    use std::io::Write;
    print!("\r\x1b[2K");
    let _ = std::io::stdout().flush();
}

/// The one place that turns a confirmed subscription into the saved plan.
fn activated(status: &license::Status) -> Result<()> {
    let tier = status.tier();
    let changed = license::set_plan(tier)?;
    let name = tier.map(|t| t.label()).unwrap_or("Anacrafter");
    println!(
        "\n  {} {}{}\n",
        paint("✓", ore::emerald()),
        bold(&paint(&format!("you're an {name}"), ore::gold())),
        match status.since {
            Some(since) => dim(&format!("  ·  since {}", since.format("%-d %b %Y"))),
            None => String::new(),
        },
    );
    if changed {
        println!(
            "  {} = {} in {}\n",
            bold("tier"),
            tier.map(|t| t.name()).unwrap_or("basic"),
            dim(&config::Config::path()?.display().to_string()),
        );
    }
    println!(
        "  {}\n",
        dim(
            "craft configure and craft watch are unlocked on every plan, \
             Slack alerts on Pro, craft mcp on Elite — and the dashboard \
             wears a gold star"
        )
    );
    Ok(())
}

fn should_skip_active_checkout(status: &license::Status, plan: Option<license::Tier>) -> bool {
    let Some(current) = status.tier() else {
        return false;
    };
    !matches!(plan, Some(target) if target > current)
}

async fn cmd_login() -> Result<()> {
    let http = reqwest::Client::new();
    let auth = auth::Auth::new(http)?;
    auth.login().await?;

    println!("  {} logged in.\n", paint("✓", ore::emerald()));

    // Register the account, so a subscription can be found from any machine —
    // and so a payment that arrived with nobody attached gets picked up here.
    // Best-effort: a lookup that is down does not make this login any less
    // valid, and the next dashboard launch tries again.
    if let Some(account) = auth::Auth::account()? {
        let _ = license::link(&account).await;
        if license::sync(&Config::load()?).await.is_some() {
            println!(
                "  {} {}\n",
                paint(theme::glyph::STAR, ore::gold()),
                dim("subscription found on this account"),
            );
        }
    }

    // A fresh login with no property selected is a dead end; nudge onward.
    let cfg = Config::load()?;
    if cfg.active_property().is_none() {
        println!("  next: {} to pick a property\n", bold("craft props"));
    }
    Ok(())
}

async fn cmd_logout() -> Result<()> {
    let http = reqwest::Client::new();
    auth::Auth::new(http)?.logout().await?;
    // The subscription record names the account that just left, and the
    // `supporter` flag beside it speaks for that account. Best-effort: the
    // sign-out has already happened and is the part that matters.
    let _ = license::forget();
    println!("  {} signed out.\n", paint("✓", ore::emerald()));
    Ok(())
}

async fn cmd_props() -> Result<()> {
    let client = Ga::new()?;
    let props = client.properties().await?;

    if props.is_empty() {
        println!("\n  no GA4 properties visible to this account.\n");
        return Ok(());
    }

    let cfg = Config::load()?;
    println!("\n{}\n", panel_top("PROPERTIES"));
    for (i, prop) in props.iter().enumerate() {
        let current = cfg.active.as_deref() == Some(prop.id.as_str());
        let marker = if current {
            paint("●", ore::emerald())
        } else {
            dim("○")
        };
        println!(
            "  {marker} {}  {}\n      {}",
            bold(&paint(&prop.name, theme::ramp(i))),
            dim(&prop.account),
            dim(&format!("id {}", prop.id)),
        );
    }
    // `use` accumulates rather than replaces, which is the only hint that a
    // rotation exists at all — worth saying once the list is non-trivial.
    println!("\n  add one with {}", bold("craft use <id>"));
    if cfg.properties.len() > 1 {
        println!(
            "  {} configured — {} between them in the dashboard\n",
            cfg.properties.len(),
            bold("tab")
        );
    } else {
        println!(
            "  {}\n",
            dim("run it again for a second property, then tab between them")
        );
    }
    println!("{}\n", panel_bottom());
    Ok(())
}

async fn cmd_use(id: &str) -> Result<()> {
    let client = Ga::new()?;
    let props = client.properties().await?;
    let wanted = id.trim().trim_start_matches("properties/");

    let found = props
        .iter()
        .find(|p| p.id == wanted)
        .with_context(|| format!("no property {wanted} on this account — run `craft props`"))?;

    let mut cfg = Config::load()?;
    cfg.upsert(&found.id, Some(found.name.clone()));
    cfg.save()?;

    println!(
        "\n  {} mining {} {}\n",
        paint("✓", ore::emerald()),
        bold(&paint(&found.name, ore::diamond())),
        dim(&format!("({})", found.id)),
    );
    Ok(())
}

/// With a name, saves it as the default. Without, lists what there is.
fn cmd_theme(name: Option<&str>) -> Result<()> {
    if let Some(name) = name {
        if !theme::select(name) {
            anyhow::bail!("no theme called {name} — run `craft theme` to list them");
        }
        let mut cfg = Config::load()?;
        cfg.theme = Some(name.to_string());
        cfg.save()?;
        println!(
            "\n  {} theme set to {}\n",
            paint("✓", ore::emerald()),
            bold(&paint(name, ore::diamond())),
        );
        return Ok(());
    }

    let current = theme::palette().name;
    let width = theme::THEMES
        .iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(0);

    println!("\n{}\n", panel_top("THEMES"));
    for palette in theme::THEMES {
        let selected = palette.name == current;
        let marker = if selected {
            paint("●", ore::emerald())
        } else {
            dim("○")
        };
        // A swatch beats a name: each row is drawn in the palette it names,
        // not in whichever one happens to be selected.
        let swatch: String = (0..8)
            .map(|i| paint("███", theme::ramp_of(palette, i)))
            .collect::<Vec<_>>()
            .join("");
        println!(
            "  {marker} {} {swatch}",
            bold(&format!("{:<width$}", palette.name))
        );
    }
    println!("\n  set one with {}\n", bold("craft theme <name>"));
    println!("{}\n", panel_bottom());
    Ok(())
}

// ---------------------------------------------------------------- reports ---

async fn cmd_overview(property: &str, days: u32, format: Format) -> Result<()> {
    let client = Ga::new()?;
    let metrics: Vec<&str> = OVERVIEW.iter().map(|m| m.api).collect();

    let current = client
        .report(
            property,
            ReportRequest::new(&metrics).range(DateRange::last_days(days)),
        )
        .await?;

    let previous = client
        .report(
            property,
            ReportRequest::new(&metrics).range(DateRange::previous_days(days)),
        )
        .await?;

    let trend = client
        .report(
            property,
            ReportRequest::new(&["totalUsers"])
                .by(&["date"])
                .range(DateRange::last_days(days)),
        )
        .await?;

    // GA returns date rows unordered; the sparkline needs them chronological,
    // and the JSON window is read off the first and last of them.
    let mut rows = trend.rows.clone();
    rows.sort_by(|a, b| a.dimension(0).cmp(b.dimension(0)));
    let daily: Vec<(String, f64)> = rows
        .iter()
        .map(|row| (row.dimension(0).to_string(), row.metric(0)))
        .collect();
    let series: Vec<f64> = daily.iter().map(|(_, users)| *users).collect();

    let empty = current.rows.is_empty() && current.totals.is_empty();
    let cfg = Config::load()?;
    let title = cfg
        .find(property)
        .map(|p| p.display())
        .unwrap_or_else(|| format!("property {property}"));

    let totals: Vec<f64> = (0..OVERVIEW.len()).map(|i| current.total(i)).collect();
    let prior: Vec<f64> = (0..OVERVIEW.len()).map(|i| previous.total(i)).collect();

    match format {
        Format::Panels => print_overview(&title, days, &totals, &prior, &series, empty),
        // One object on one line, so a shell pipes it straight into curl or jq
        // with nothing to strip first. Both of these print the numbers and
        // nothing else: a progress line or a hint on stdout would be a parse
        // error at the other end of the pipe.
        machine => {
            let overview = Overview {
                property,
                title: &title,
                days,
                totals: &totals,
                prior: &prior,
                daily: &daily,
                empty,
            };
            let payload = match machine {
                Format::Slack => report::slack(&overview),
                _ => report::json(&overview),
            };
            println!("{payload}");
        }
    }
    Ok(())
}

/// Shared by `overview` and `demo` so both render through one code path.
fn print_overview(
    title: &str,
    days: u32,
    current: &[f64],
    previous: &[f64],
    series: &[f64],
    empty: bool,
) {
    println!(
        "\n{}\n",
        panel_top(&format!("{} · last {days} days", title.to_uppercase()))
    );

    if empty {
        println!("  {}\n", dim("no data in this window."));
        println!("{}\n", panel_bottom());
        return;
    }

    for (i, metric) in OVERVIEW.iter().enumerate() {
        let now = current.get(i).copied().unwrap_or(0.0);
        let before = previous.get(i).copied().unwrap_or(0.0);
        print!("{}", render::metric_block(metric, now, before));
    }

    if series.len() > 1 {
        println!(
            "  {}  {}\n",
            dim("daily villagers"),
            render::sparkline(series, ore::grass())
        );
    }

    let snap = achievements::Snapshot {
        users: current[0],
        prev_users: previous[0],
        sessions: current[1],
        views: current[2],
        conversions: current[3],
        prev_conversions: previous[3],
        bounce_rate: current[4],
        prev_bounce_rate: previous[4],
        avg_duration: current[5],
        daily_users: series.to_vec(),
    };

    for achievement in achievements::unlocked(&snap).iter().take(3) {
        print!("{}", render::toast(achievement.title, &achievement.detail));
    }

    println!("\n{}\n", panel_bottom());
}

/// Synthetic numbers shaped like a small site having a good week, so the theme
/// can be evaluated (and screenshotted) before anyone connects an account.
fn cmd_demo() -> Result<()> {
    let series = vec![1402.0, 1288.0, 1531.0, 1495.0, 1760.0, 1834.0, 1971.0];
    let current = vec![12_481.0, 18_203.0, 41_776.0, 312.0, 0.412, 214.0];
    let previous = vec![11_450.0, 17_004.0, 39_210.0, 258.0, 0.478, 191.0];

    print_overview(
        "Contoso Labs (demo)",
        7,
        &current,
        &previous,
        &series,
        false,
    );

    println!(
        "  {}\n",
        dim("synthetic data — run `craft login` to connect a real property")
    );
    Ok(())
}

/// Shared implementation for pages / portals / realms — same shape, different
/// dimension and metric.
async fn cmd_ranked(
    property: &str,
    days: u32,
    limit: i32,
    dimension: &str,
    metric: &str,
    unit: &str,
    title: &str,
) -> Result<()> {
    let client = Ga::new()?;
    let report = client
        .report(
            property,
            ReportRequest::new(&[metric])
                .by(&[dimension])
                .range(DateRange::last_days(days))
                .top(metric, limit),
        )
        .await?;

    println!("\n{}\n", panel_top(&format!("{title} · last {days} days")));

    if report.is_empty() {
        println!("  {}\n", dim("no data in this window."));
        println!("{}\n", panel_bottom());
        return Ok(());
    }

    println!(
        "  {}\n",
        dim(&format!(
            "{} ranked by {}",
            theme::dimension_label(dimension),
            unit
        ))
    );

    let rows: Vec<(String, f64)> = report
        .rows
        .iter()
        .map(|r| (r.dimension(0).to_string(), r.metric(0)))
        .collect();

    print!("{}", render::ranked_table(&rows, unit, 26));
    println!("\n{}\n", panel_bottom());
    Ok(())
}

async fn cmd_live(property: &str) -> Result<()> {
    let client = Ga::new()?;
    let report = client
        .realtime(
            property,
            ReportRequest::new(&["activeUsers"])
                .by(&["country"])
                .top("activeUsers", 10),
        )
        .await?;

    let total: f64 = report.rows.iter().map(|r| r.metric(0)).sum();

    println!("\n{}\n", panel_top("RIGHT NOW"));
    println!(
        "  {} {}\n",
        bold(&paint(&render::commas(total), ore::xp())),
        dim("players online (last 30 min)"),
    );

    if !report.is_empty() {
        let rows: Vec<(String, f64)> = report
            .rows
            .iter()
            .map(|r| (r.dimension(0).to_string(), r.metric(0)))
            .collect();
        print!("{}", render::ranked_table(&rows, "online", 26));
    }

    println!("\n{}\n", panel_bottom());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_for(tier: Option<license::Tier>) -> license::Status {
        license::Status {
            status: "active".into(),
            since: None,
            subscribed: Some(true),
            founder: None,
            tier: tier.map(|t| t.name().to_string()),
        }
    }

    #[test]
    fn basic_subscribers_can_upgrade_to_higher_plans() {
        let basic = status_for(Some(license::Tier::Basic));
        assert!(!should_skip_active_checkout(
            &basic,
            Some(license::Tier::Pro)
        ));
        assert!(!should_skip_active_checkout(
            &basic,
            Some(license::Tier::Elite)
        ));

        let pro = status_for(Some(license::Tier::Pro));
        assert!(should_skip_active_checkout(&pro, Some(license::Tier::Pro)));
        assert!(!should_skip_active_checkout(
            &pro,
            Some(license::Tier::Elite)
        ));
    }
}
