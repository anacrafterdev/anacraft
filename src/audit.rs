//! `craft audit` — whether the numbers are worth reading, before anybody reads
//! them.
//!
//! Every other command in here answers "what happened". This one answers the
//! question that comes before it: is this property measuring the site at all,
//! and is what it measured worth trusting. That is the thing people buy a
//! fixed-price analytics audit for, and most of it is a handful of API reads
//! and a set of thresholds rather than an afternoon of somebody's time.
//!
//! Three rules it holds to.
//!
//! It reports a symptom and names the usual cause, never the other way round.
//! "Bounce rate is 1.2%" is something the API said; "you have two page_view
//! tags" is a guess, and a report that states guesses as findings gets
//! believed once and then ignored.
//!
//! Every check has a floor under it. A property with eighty sessions has no
//! meaningful bounce rate, no meaningful direct share and no meaningful
//! anything else — firing on one is how an audit becomes a list of things to
//! dismiss.
//!
//! And configuration and measurement are read separately, because they fail
//! separately. The Data API says how often `purchase` fired; the Admin API
//! says whether anybody ever told GA4 that `purchase` was the point. A site
//! can pass the first and fail the second for a year without noticing, and
//! that combination — traffic arriving, nothing marked as an outcome — is the
//! single most common thing this finds.

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::Result;
use ratatui::style::Color;
use serde_json::{json, Value};

use crate::config::Config;
use crate::ga::{DateRange, Ga, KeyEvent, ReportRequest, WebStream};
use crate::render::{self, bold, dim, paint, panel_bottom, panel_top};
use crate::report::Format;
use crate::theme::{glyph, ore};

/// The window an audit reads by default.
///
/// Four weeks rather than the dashboard's seven days, for the same reason
/// `watch` averages over four: a key event that did not fire this week may
/// simply not have happened, and one that did not fire in a month is broken.
/// It also covers every weekday four times, so a site that is quiet at
/// weekends does not read as a site that stopped.
pub const DEFAULT_DAYS: u32 = 28;

/// Sessions a property needs before any share or rate is worth judging. Under
/// this a single visitor moves a percentage by more than the thresholds below,
/// so the ratio checks would be reporting noise with a decimal point on it.
const MIN_SESSIONS: f64 = 100.0;

/// A bounce rate under this is not a good website.
const FLAT_BOUNCE: f64 = 0.05;

/// ...but only alongside this, because a metric that failed to come back also
/// reads as zero. Two page views per session is the other half of the same
/// symptom — a `page_view` that fires twice doubles the views and engages
/// every session — and requiring both is what keeps an absent metric from
/// being reported as a defect.
const BUSY_SESSION: f64 = 2.0;

/// How much an event had to have been doing before its silence is news. Below
/// this it is a rare event having a quiet month, not a tag that was removed.
const SILENT_BEFORE: f64 = 30.0;

/// Share of sessions GA4 could not attribute at all before it is a finding.
const NOT_SET_SHARE: f64 = 0.05;

/// Share of sessions landing in direct before it is worth a second look. High,
/// and deliberately: plenty of sites are genuinely reached by people typing
/// their name, which is why this one is a note rather than a warning.
const DIRECT_SHARE: f64 = 0.70;

/// How many names a finding lists before it stops and counts the rest.
const NAMED: usize = 4;

/// Every check this command makes, whether or not it finds anything. The count
/// is part of the report: "no findings" means nothing unless it says how many
/// ways it looked.
const CHECKS: &[&str] = &[
    "collecting",
    "data_stream",
    "key_events_configured",
    "key_events_firing",
    "revenue_tagged",
    "double_counting",
    "self_referral",
    "payment_handoff",
    "event_silence",
    "event_names",
    "source_not_set",
    "direct_share",
];

/// The checks that are statements about data, and so have nothing to say about
/// a property that recorded none. When the window comes back empty these are
/// reported as not run rather than as passes — nine findings saying "no data"
/// would be nine ways of saying it once, and nine passes would be worse.
const NEEDS_DATA: &[&str] = &[
    "key_events_firing",
    "revenue_tagged",
    "double_counting",
    "self_referral",
    "payment_handoff",
    "event_silence",
    "event_names",
    "source_not_set",
    "direct_share",
];

/// Hosts that have no business appearing as a referrer.
///
/// A payment or sign-in page the visitor was sent to and came back from is a
/// handoff, not an acquisition — but GA4 cannot tell the difference unless the
/// domain is listed as an unwanted referral, so the return trip starts a new
/// session and the conversion is credited to the gateway. Matched on the
/// registrable domain and its subdomains, so `checkout.stripe.com` is caught
/// by `stripe.com`.
const HANDOFF_HOSTS: &[&str] = &[
    "stripe.com",
    "paypal.com",
    "braintreegateway.com",
    "authorize.net",
    "squareup.com",
    "checkout.com",
    "adyen.com",
    "klarna.com",
    "mollie.com",
    "razorpay.com",
    "accounts.google.com",
    "login.microsoftonline.com",
    "auth0.com",
];

/// The totals one request covers, and where each lands in the response.
const TOTALS: &[&str] = &["sessions", "screenPageViews", "bounceRate", "totalRevenue"];
const T_SESSIONS: usize = 0;
const T_VIEWS: usize = 1;
const T_BOUNCE: usize = 2;
const T_REVENUE: usize = 3;

/// The event GA4 reads money off. Named rather than inferred: a site can call
/// its own checkout anything, but only `purchase` populates `totalRevenue`.
const PURCHASE: &str = "purchase";

/// The color down the side of a Slack message. Fixed rather than read from the
/// palette, for the reason `watch` gives: this is going to a workspace where
/// the theme selected on this machine is not a thing that exists.
const BAR_CLEAR: &str = "#2f9e44";

// ---------------------------------------------------------------- findings ---

/// How much a finding matters. Ordered, so sorting puts the things that stop
/// a report being true above the things that make it untidy.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Grade {
    /// A number somewhere is wrong, not just unflattering.
    Critical,
    /// The numbers are real but something is distorting them.
    Warning,
    /// Worth knowing before reading anything else. Not necessarily a defect.
    Note,
}

impl Grade {
    fn slug(self) -> &'static str {
        match self {
            Grade::Critical => "critical",
            Grade::Warning => "warning",
            Grade::Note => "note",
        }
    }

    /// Ore grades, so severity reads at a glance in the same texture as
    /// everything else: solid block, cracked, barely there.
    fn glyph(self) -> char {
        match self {
            Grade::Critical => glyph::FULL,
            Grade::Warning => glyph::PARTIAL,
            Grade::Note => glyph::CRACKED,
        }
    }

    fn color(self) -> Color {
        match self {
            Grade::Critical => ore::redstone(),
            Grade::Warning => ore::gold(),
            Grade::Note => ore::stone(),
        }
    }

    /// The bar down the side of a Slack attachment.
    fn bar(self) -> &'static str {
        match self {
            Grade::Critical => crate::watch::BAR_ALARM,
            Grade::Warning => crate::watch::BAR_WATCH,
            Grade::Note => BAR_CLEAR,
        }
    }

    /// How a count of these reads in a sentence.
    fn tally(self, n: usize) -> String {
        match self {
            Grade::Critical => format!("{n} critical"),
            Grade::Warning => format!("{n} warning{}", if n == 1 { "" } else { "s" }),
            Grade::Note => format!("{n} note{}", if n == 1 { "" } else { "s" }),
        }
    }
}

/// One thing wrong, and what it costs.
pub struct Finding {
    /// Stable slug, so a script can match on a check without parsing English.
    check: &'static str,
    grade: Grade,
    /// The defect in one line, as a symptom.
    headline: String,
    /// What it means and what fixes it. The part somebody is paying a
    /// consultant for.
    detail: String,
    /// The number or names the finding rests on. Separate from the headline so
    /// a reader can tell the claim from the evidence for it.
    evidence: Option<String>,
}

/// One pass over one property.
pub struct Audit {
    property: String,
    title: String,
    days: u32,
    /// How many of [`CHECKS`] actually ran. Lower when the Admin API could not
    /// be read, or when the property turned out to be recording nothing and
    /// the measurement checks had nothing to measure.
    checks: usize,
    findings: Vec<Finding>,
}

impl Audit {
    pub fn clean(&self) -> bool {
        self.findings.is_empty()
    }

    fn count(&self, grade: Grade) -> usize {
        self.findings.iter().filter(|f| f.grade == grade).count()
    }

    /// The worst thing found, which is what colors the Slack bar.
    fn worst(&self) -> Option<Grade> {
        self.findings.iter().map(|f| f.grade).min()
    }

    /// Criticals first, and within a grade the order the checks ran in.
    fn sorted(mut self) -> Audit {
        self.findings.sort_by_key(|f| f.grade);
        self
    }
}

// ------------------------------------------------------------------ checks ---

/// Read the property and grade what comes back.
async fn examine(ga: &Ga, property: &str, title: &str, days: u32) -> Result<Audit> {
    // Four reports, one round trip. Nothing here depends on anything else
    // here, and an audit that took four sequential round trips would be four
    // times as slow for no extra truth.
    let (totals, events, before, sources) = tokio::try_join!(
        ga.report(
            property,
            ReportRequest::new(TOTALS).range(DateRange::last_days(days))
        ),
        ga.report(
            property,
            ReportRequest::new(&["eventCount"])
                .by(&["eventName"])
                .top("eventCount", 200)
                .range(DateRange::last_days(days))
        ),
        ga.report(
            property,
            ReportRequest::new(&["eventCount"])
                .by(&["eventName"])
                .top("eventCount", 200)
                .range(DateRange::previous_days(days))
        ),
        ga.report(
            property,
            ReportRequest::new(&["sessions"])
                .by(&["sessionSource"])
                .top("sessions", 200)
                .range(DateRange::last_days(days))
        ),
    )?;

    // Configuration, and `join` rather than `try_join`: these two are a
    // different API with a different permission behind them, and an account
    // that can report but not read Admin should still get the ten checks that
    // do not need it rather than an error instead of an audit.
    let (streams, keys) = tokio::join!(ga.web_streams(property), ga.key_events(property));

    let mut findings = Vec::new();
    let mut skipped: Vec<&'static str> = Vec::new();

    let sessions = totals.total(T_SESSIONS);
    let views = totals.total(T_VIEWS);
    let bounce = totals.total(T_BOUNCE);
    let revenue = totals.total(T_REVENUE);

    let counts = tally(&events);
    let previously = tally(&before);

    // --- collecting -------------------------------------------------------
    //
    // Everything downstream of this is a statement about data that is not
    // there. Eight more findings saying so would be eight ways of saying it
    // once, so they are recorded as not run instead.
    let dark = counts.is_empty() && sessions <= 0.0;
    if dark {
        findings.push(Finding {
            check: "collecting",
            grade: Grade::Critical,
            headline: "nothing recorded at all".into(),
            detail: format!(
                "{days} days without a single event. Either the tag is not on the site, \
                 or {property} is not the property the site reports to — a measurement id \
                 copied from a second property looks exactly like this from here. \
                 `craft live` on a page you have open is the quickest way to tell them apart."
            ),
            evidence: None,
        });
        skipped.extend_from_slice(NEEDS_DATA);
    }

    // --- data_stream ------------------------------------------------------
    match &streams {
        Err(_) => skipped.push("data_stream"),
        Ok(list) if list.is_empty() => findings.push(Finding {
            check: "data_stream",
            grade: Grade::Critical,
            headline: "no web data stream".into(),
            detail: "this property has no web stream, so there is no measurement id to put \
                     on a site and nothing can ever report into it. `craft configure \
                     <domain>` creates the stream and prints the tag."
                .into(),
            evidence: None,
        }),
        Ok(list) if list.len() > 1 => findings.push(Finding {
            check: "data_stream",
            grade: Grade::Note,
            headline: "more than one site reports here".into(),
            detail: "every number on this property is the sum of all of its streams. That \
                     is what you want for one site on several domains, and not what you \
                     want for a staging site sharing a property with production."
                .into(),
            evidence: Some(
                list.iter()
                    .map(stream_label)
                    .collect::<Vec<_>>()
                    .join(" · "),
            ),
        }),
        Ok(_) => {}
    }

    // --- key_events_configured --------------------------------------------
    //
    // The one that pays for the command. A property can collect flawlessly for
    // a year and still answer no question anybody has, because nothing in it
    // was ever marked as the point.
    match &keys {
        Err(_) => {
            skipped.push("key_events_configured");
            skipped.push("key_events_firing");
        }
        Ok(list) if list.is_empty() => findings.push(Finding {
            check: "key_events_configured",
            grade: Grade::Critical,
            headline: "nothing is marked as a key event".into(),
            detail: "GA4 is counting traffic and nothing else. With no key event, no report \
                     on this property can say whether any of that traffic was worth having, \
                     and Google Ads has nothing to import or optimise against. Mark the \
                     events that matter under Admin → Events."
                .into(),
            evidence: None,
        }),
        Ok(_) => {}
    }

    // --- key_events_firing ------------------------------------------------
    if let (Ok(list), false) = (&keys, dark) {
        let mut dead: Vec<&KeyEvent> = list
            .iter()
            .filter(|k| counts.get(&k.name).copied().unwrap_or(0.0) <= 0.0)
            .collect();
        // Hand-defined ones first. A custom key event that never fires is
        // someone's deliberate setup not working; one GA4 created itself may
        // simply not apply to this site, and the list is capped — so the
        // names that survive the cap should be the ones worth chasing.
        dead.sort_by_key(|k| !k.custom);

        if !dead.is_empty() {
            let names: Vec<&str> = dead.iter().map(|k| k.name.as_str()).collect();
            findings.push(Finding {
                check: "key_events_firing",
                grade: Grade::Critical,
                headline: format!(
                    "{} never fired",
                    plural(dead.len(), "a key event", "key events")
                ),
                detail: format!(
                    "configured as an outcome and not recorded once in {days} days. Either \
                     the event is not being sent at all, or it is being sent under a \
                     different name — GA4 matches names exactly, so `Purchase` and \
                     `purchase` are two unrelated events and only one of them counts."
                ),
                evidence: Some(listing(&names)),
            });
        }
    }

    // --- revenue_tagged ---------------------------------------------------
    let purchases = counts.get(PURCHASE).copied().unwrap_or(0.0);
    if !dark && purchases > 0.0 && revenue <= 0.0 {
        findings.push(Finding {
            check: "revenue_tagged",
            grade: Grade::Critical,
            headline: "purchases arrive without their money".into(),
            detail: "`purchase` is firing and the property has recorded no revenue at all, \
                     which happens when the event is sent without its `value` and `currency` \
                     parameters. Every revenue, ARPU and ROAS figure here is zero as a \
                     result — including in any Google Ads account importing conversions \
                     from this property, where it means bidding is optimising against a \
                     number that is always nothing."
                .into(),
            evidence: Some(format!(
                "{} purchases · {} revenue",
                render::commas(purchases),
                render::commas(revenue)
            )),
        });
    }

    // --- double_counting --------------------------------------------------
    let per_session = if sessions > 0.0 {
        views / sessions
    } else {
        0.0
    };
    if !dark && sessions >= MIN_SESSIONS && bounce < FLAT_BOUNCE && per_session >= BUSY_SESSION {
        findings.push(Finding {
            check: "double_counting",
            grade: Grade::Warning,
            headline: "page views look counted twice".into(),
            detail: "a bounce rate this low with this many views per session is the shape a \
                     duplicate `page_view` makes: the second one engages every session that \
                     would otherwise have bounced, and doubles the views while it is at it. \
                     Usually a gtag.js snippet left in the page beside a GTM tag that also \
                     sends one. Check the page in GA4's DebugView — two page_view rows on \
                     one load is the whole diagnosis."
                .into(),
            evidence: Some(format!(
                "{:.1}% bounce · {per_session:.1} views/session",
                bounce * 100.0
            )),
        });
    }

    // --- self_referral and payment_handoff --------------------------------
    if !dark {
        let referrers = sources_by_host(&sources);

        let own: Vec<String> = streams
            .iter()
            .flatten()
            .filter_map(|s| host_of(&s.default_uri))
            .collect();
        if streams.is_err() {
            skipped.push("self_referral");
        } else if let Some(hit) = referrers
            .iter()
            .find(|(host, _)| own.iter().any(|mine| under(host, mine)))
        {
            findings.push(Finding {
                check: "self_referral",
                grade: Grade::Warning,
                headline: "the site refers itself".into(),
                detail: "sessions are arriving with the site's own domain as their source, \
                         which is a visit being cut in half rather than a visit arriving. \
                         It happens when a second domain or subdomain is not covered by the \
                         property's cross-domain configuration: the visitor crosses, GA4 \
                         sees a new referrer, and starts a fresh session that credits the \
                         site for its own traffic. Admin → Data Streams → Configure tag \
                         settings → Configure your domains."
                    .into(),
                evidence: Some(format!("{} · {} sessions", hit.0, render::commas(hit.1))),
            });
        }

        let handoffs: Vec<&(String, f64)> = referrers
            .iter()
            .filter(|(host, _)| HANDOFF_HOSTS.iter().any(|known| under(host, known)))
            .collect();
        if !handoffs.is_empty() {
            let names: Vec<&str> = handoffs.iter().map(|(host, _)| host.as_str()).collect();
            findings.push(Finding {
                check: "payment_handoff",
                grade: Grade::Warning,
                headline: "a payment page is credited with conversions".into(),
                detail: "a visitor sent out to pay or sign in and returned starts a new \
                         session referred by the gateway, and whatever actually brought them \
                         loses the conversion to it. The traffic is real, the attribution is \
                         not. List these under Admin → Data Streams → Configure tag settings \
                         → List unwanted referrals."
                    .into(),
                evidence: Some(listing(&names)),
            });
        }
    }

    // --- event_silence ----------------------------------------------------
    if !dark {
        let mut gone: Vec<(&String, f64)> = previously
            .iter()
            .filter(|(name, was)| {
                **was >= SILENT_BEFORE && counts.get(*name).copied().unwrap_or(0.0) <= 0.0
            })
            .map(|(name, was)| (name, *was))
            .collect();
        // Loudest first: an event that was doing thousands going quiet is a
        // different-sized problem from one that was doing thirty.
        gone.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        if !gone.is_empty() {
            let shown: Vec<String> = gone
                .iter()
                .map(|(name, was)| format!("{name} {} → 0", render::commas(*was)))
                .collect();
            findings.push(Finding {
                check: "event_silence",
                grade: Grade::Warning,
                headline: format!(
                    "{} stopped firing",
                    plural(gone.len(), "an event", "events")
                ),
                detail: format!(
                    "recorded in the {days} days before this window and not once inside it. \
                     An event that stops between one release and the next is a tag that was \
                     removed, renamed, or moved behind something that no longer runs — and \
                     nothing in GA4 will say so, because an event that is not sent leaves no \
                     trace of having been expected."
                ),
                evidence: Some(listing(
                    &shown.iter().map(String::as_str).collect::<Vec<_>>(),
                )),
            });
        }
    }

    // --- event_names ------------------------------------------------------
    if !dark {
        let collisions = collisions(&counts);
        if !collisions.is_empty() {
            findings.push(Finding {
                check: "event_names",
                grade: Grade::Warning,
                headline: "one event under two names".into(),
                detail: "GA4 matches event names exactly, so these are collected, reported \
                         and marked as key events separately — every report that names one \
                         of them is missing the other's traffic. Usually a leftover from an \
                         older tag that was rewritten rather than removed."
                    .into(),
                evidence: Some(listing(
                    &collisions.iter().map(String::as_str).collect::<Vec<_>>(),
                )),
            });
        }
    }

    // --- source_not_set and direct_share ----------------------------------
    if !dark {
        let total: f64 = sources.rows.iter().map(|row| row.metric(0)).sum();
        let share = |needle: &str| -> f64 {
            if total <= 0.0 {
                return 0.0;
            }
            sources
                .rows
                .iter()
                .filter(|row| row.dimension(0) == needle)
                .map(|row| row.metric(0))
                .sum::<f64>()
                / total
        };

        if total < MIN_SESSIONS {
            // Both of the checks below are shares of this number, and a share
            // of eighty sessions is not a measurement.
            skipped.push("source_not_set");
            skipped.push("direct_share");
        } else {
            let not_set = share("(not set)");
            if not_set >= NOT_SET_SHARE {
                findings.push(Finding {
                    check: "source_not_set",
                    grade: Grade::Warning,
                    headline: "sessions with no source at all".into(),
                    detail: "`(not set)` is GA4 saying it could not attribute the session to \
                             anything — not direct, not unknown, absent. It usually means \
                             hits arriving without a page context: a Measurement Protocol \
                             event sent without a source, or a tag firing before the page it \
                             is on exists. These sessions can never be credited to a channel, \
                             so every acquisition report is missing them."
                        .into(),
                    evidence: Some(format!("{:.0}% of sessions", not_set * 100.0)),
                });
            }

            let direct = share("(direct)");
            if direct >= DIRECT_SHARE {
                findings.push(Finding {
                    check: "direct_share",
                    grade: Grade::Note,
                    headline: "almost everything arrives as direct".into(),
                    detail: "direct is where GA4 puts a session with no referrer to read. A \
                             share this high is normal for a site people reach by typing its \
                             name, and otherwise points at campaigns going out without UTM \
                             tags, a redirect that drops the referrer on the way in, or a \
                             consent banner that holds the tag back until after the landing \
                             page. Worth knowing which, before reading any channel report."
                        .into(),
                    evidence: Some(format!("{:.0}% of sessions", direct * 100.0)),
                });
            }
        }
    }

    // A check can be skipped for two reasons at once — a dark property whose
    // Admin API is also unreadable skips `key_events_firing` twice — and
    // counting it twice would report fewer checks run than were skipped.
    skipped.sort_unstable();
    skipped.dedup();

    Ok(Audit {
        property: property.to_string(),
        title: title.to_string(),
        days,
        checks: CHECKS.len().saturating_sub(skipped.len()),
        findings,
    }
    .sorted())
}

// ------------------------------------------------------------------ helpers ---

/// A `name → count` view of a one-dimension report.
fn tally(report: &crate::ga::Report) -> BTreeMap<String, f64> {
    report
        .rows
        .iter()
        .map(|row| (row.dimension(0).to_string(), row.metric(0)))
        .collect()
}

/// The session sources that look like hostnames, with their session counts.
///
/// `sessionSource` mixes hosts with words — `google`, `(direct)`, `newsletter`
/// — and only the hosts can be matched against a domain. A value with no dot in
/// it is not one.
fn sources_by_host(report: &crate::ga::Report) -> Vec<(String, f64)> {
    report
        .rows
        .iter()
        .filter(|row| row.metric(0) > 0.0)
        .filter_map(|row| Some((host_of(row.dimension(0))?, row.metric(0))))
        .collect()
}

/// The hostname in a URI, a bare host, or nothing.
///
/// Lowercased, with the scheme, port, path and a leading `www.` removed, so
/// `https://www.Example.com/pricing` and `example.com` are the same host —
/// which is the comparison every domain check here wants to make.
fn host_of(raw: &str) -> Option<String> {
    let rest = raw.split_once("://").map_or(raw, |(_, rest)| rest);
    let host = rest
        .split('/')
        .next()?
        .split(':')
        .next()?
        .trim()
        .to_ascii_lowercase();
    let host = host.trim_start_matches("www.");
    // A source with no dot is a channel name, not a domain.
    if host.is_empty() || !host.contains('.') {
        return None;
    }
    Some(host.to_string())
}

/// Whether `host` is `domain` or something under it. Suffix-matched on a label
/// boundary, so `stripe.com` catches `checkout.stripe.com` and does not catch
/// `notstripe.com`.
fn under(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Event names that differ only in punctuation or case, grouped.
///
/// Normalising to letters and digits alone is what catches the real pairs —
/// `sign_up` against `signUp`, `add-to-cart` against `add_to_cart` — without
/// pulling in similar-but-different events like `sign_up` and `sign_up_failed`,
/// which are two events on purpose.
fn collisions(counts: &BTreeMap<String, f64>) -> Vec<String> {
    let mut groups: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for name in counts.keys() {
        let key: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect();
        if key.is_empty() {
            continue;
        }
        groups.entry(key).or_default().push(name);
    }
    groups
        .into_values()
        .filter(|names| names.len() > 1)
        .map(|mut names| {
            // Busiest spelling first, so the pair reads as the event and then
            // the stray variant leaking away from it, rather than as two
            // names in whatever order the map happened to sort them.
            names.sort_by(|a, b| {
                let (x, y) = (counts.get(*a), counts.get(*b));
                y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal)
            });
            names.join(" / ")
        })
        .collect()
}

/// A stream, as a person would name it: its domain where it has one, its
/// measurement id where it does not.
fn stream_label(stream: &WebStream) -> String {
    host_of(&stream.default_uri).unwrap_or_else(|| stream.measurement_id.clone())
}

/// The first few of something, and a count of the rest.
fn listing(items: &[&str]) -> String {
    if items.len() <= NAMED {
        return items.join(", ");
    }
    format!(
        "{}, and {} more",
        items[..NAMED].join(", "),
        items.len() - NAMED
    )
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 { one } else { many }.to_string()
}

/// Wrap on words, counting characters rather than bytes — the panel is 62
/// columns of display width, and an event name can be anything.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

// ---------------------------------------------------------------- rendering ---

/// The panel width less the indent the detail lines sit at.
const TEXT_WIDTH: usize = render::PANEL_WIDTH - 6;

/// Panels, for a person at a terminal or reading cron's mail.
fn panels(audit: &Audit) -> String {
    let mut out = format!(
        "\n{}\n\n",
        panel_top(&format!(
            "AUDIT · {} · {} DAYS",
            audit.title.to_uppercase(),
            audit.days
        ))
    );

    if audit.clean() {
        out.push_str(&format!("  {}\n", paint("nothing to fix.", ore::emerald())));
        for line in wrap(
            "every check passed. That is a statement about how the property is \
             configured and what it collected, not about how the site is doing — \
             `craft overview` answers that.",
            TEXT_WIDTH,
        ) {
            out.push_str(&format!("    {}\n", dim(&line)));
        }
        out.push('\n');
    }

    for finding in &audit.findings {
        out.push_str(&format!(
            "  {} {}\n",
            paint(&finding.grade.glyph().to_string(), finding.grade.color()),
            bold(&paint(
                &finding.headline.to_uppercase(),
                finding.grade.color()
            )),
        ));
        if let Some(evidence) = &finding.evidence {
            for line in wrap(evidence, TEXT_WIDTH) {
                out.push_str(&format!("    {}\n", paint(&line, finding.grade.color())));
            }
        }
        for line in wrap(&finding.detail, TEXT_WIDTH) {
            out.push_str(&format!("    {}\n", dim(&line)));
        }
        out.push('\n');
    }

    out.push_str(&format!("  {}\n", dim(&summary(audit))));
    out.push_str(&format!("{}\n", panel_bottom()));
    out
}

/// The line that says how hard it looked. "No findings" is only worth
/// anything next to the number of ways it tried to find something.
fn summary(audit: &Audit) -> String {
    let mut parts = vec![format!(
        "{} check{} run",
        audit.checks,
        if audit.checks == 1 { "" } else { "s" }
    )];
    for grade in [Grade::Critical, Grade::Warning, Grade::Note] {
        let n = audit.count(grade);
        if n > 0 {
            parts.push(grade.tally(n));
        }
    }
    if audit.findings.is_empty() {
        parts.push("nothing found".to_string());
    }
    if audit.checks < CHECKS.len() {
        parts.push(format!(
            "{} not run",
            CHECKS.len().saturating_sub(audit.checks)
        ));
    }
    parts.join(" · ")
}

/// The findings alone — no property, no window.
///
/// What `audit_site` answers with over MCP, where the envelope already carries
/// both, and what `as_json` wraps for the pipe. One shape computed once, for
/// the reason `report` gives about `status_payload`: the report an assistant
/// reads and the report a script parses must not be able to drift into
/// disagreeing about the same property.
pub(crate) fn findings_payload(audit: &Audit) -> Value {
    json!({
        "checks_run": audit.checks,
        "checks_available": CHECKS.len(),
        "clean": audit.clean(),
        "counts": {
            "critical": audit.count(Grade::Critical),
            "warning": audit.count(Grade::Warning),
            "note": audit.count(Grade::Note),
        },
        "findings": audit.findings.iter().map(|f| json!({
            "check": f.check,
            "grade": f.grade.slug(),
            "headline": f.headline,
            "detail": f.detail,
            "evidence": f.evidence,
        })).collect::<Vec<_>>(),
    })
}

/// One object, shaped the way `overview --format json` and `watch --format
/// json` are: the findings, plus which property and when.
fn as_json(audit: &Audit) -> Value {
    let mut payload = findings_payload(audit);
    if let Some(object) = payload.as_object_mut() {
        object.insert("property".into(), json!(audit.property));
        object.insert("title".into(), json!(audit.title));
        object.insert("url".into(), json!(crate::watch::ga_url(&audit.property)));
        object.insert("days".into(), json!(audit.days));
    }
    payload
}

/// The `audit_site` tool, over an authenticated client.
pub(crate) async fn inspect(ga: &Ga, property: &str, title: &str, days: u32) -> Result<Value> {
    Ok(findings_payload(&examine(ga, property, title, days).await?))
}

/// The same tool, on synthetic data.
pub(crate) fn inspect_demo(days: u32) -> Value {
    findings_payload(&demo_audit(days).sorted())
}

/// A Block Kit payload, for a webhook or a pipe into curl.
///
/// Unlike `watch`, this renders on a clean pass too. A watch posting "nothing
/// happened" every hour trains people to ignore the channel; an audit is a
/// thing somebody asked for, and "twelve checks, nothing found" is the answer
/// they asked for.
fn as_slack(audit: &Audit) -> Value {
    let heading = if audit.clean() {
        format!("⛏ {} · audit clean", audit.title)
    } else {
        format!(
            "⛏ {} · {} finding{}",
            audit.title,
            audit.findings.len(),
            if audit.findings.len() == 1 { "" } else { "s" }
        )
    };

    let mut body = vec![json!({
        "type": "header",
        "text": { "type": "plain_text", "text": cut(&heading, 150) },
    })];

    for finding in &audit.findings {
        let evidence = finding
            .evidence
            .as_deref()
            .map(|e| format!("\n`{e}`"))
            .unwrap_or_default();
        body.push(json!({
            "type": "section",
            "text": { "type": "mrkdwn", "text": cut(&format!(
                "*{} — {}*{}\n{}",
                finding.grade.slug(),
                finding.headline,
                evidence,
                finding.detail,
            ), 3000) },
        }));
    }

    body.push(json!({
        "type": "context",
        "elements": [{
            "type": "mrkdwn",
            "text": format!(
                "{} · last {} days · <{}|open in GA4>",
                summary(audit),
                audit.days,
                crate::watch::ga_url(&audit.property),
            ),
        }],
    }));

    json!({
        "text": heading,
        "attachments": [{
            "color": audit.worst().unwrap_or(Grade::Note).bar(),
            "blocks": body,
        }],
    })
}

/// Character-wise, because a Block Kit limit is in characters and slicing a
/// multi-byte name at a byte index panics.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars()
        .take(max.saturating_sub(1))
        .chain(['…'])
        .collect()
}

// ------------------------------------------------------------------- entry ---

pub struct Options {
    pub days: u32,
    pub format: Format,
    pub demo: bool,
}

/// Audit once and exit.
pub async fn run(cfg: &Config, property: Option<&str>, opts: Options) -> Result<()> {
    let days = opts.days.max(1);

    if opts.demo {
        // The shop window, same as everywhere else: what the report looks like,
        // before anything is paid for or connected.
        let audit = demo_audit(days).sorted();
        emit(&audit, opts.format);
        return finish(&audit);
    }

    let tier = crate::license::sync(cfg).await;
    let cfg = &Config::load().unwrap_or_default();
    crate::license::gate(tier, crate::license::Tier::Pro, "craft audit")
        .map_err(|reason| anyhow::anyhow!(reason))?;

    let id = cfg.resolve_property(property)?;
    let title = cfg
        .find(&id)
        .map(|p| p.display())
        .unwrap_or_else(|| format!("property {id}"));

    let audit = examine(&Ga::new()?, &id, &title, days).await?;
    emit(&audit, opts.format);
    finish(&audit)
}

fn emit(audit: &Audit, format: Format) {
    match format {
        Format::Panels => print!("{}", panels(audit)),
        Format::Json => println!("{}", as_json(audit)),
        Format::Slack => println!("{}", as_slack(audit)),
    }
}

/// `0` when the property is clean, `2` when it is not, so a shell can gate on
/// it the way it gates on `craft watch`.
fn finish(audit: &Audit) -> Result<()> {
    if audit.clean() {
        return Ok(());
    }
    std::io::stdout().flush().ok();
    std::process::exit(2);
}

// -------------------------------------------------------------------- demo ---

/// One of each grade, so `--demo` shows the whole shape of a report.
fn demo_audit(days: u32) -> Audit {
    Audit {
        property: "397412345".to_string(),
        title: "Contoso Labs (demo)".to_string(),
        days,
        checks: CHECKS.len(),
        findings: vec![
            Finding {
                check: "key_events_firing",
                grade: Grade::Critical,
                headline: "key events never fired".into(),
                detail: format!(
                    "configured as an outcome and not recorded once in {days} days. Either \
                     the event is not being sent at all, or it is being sent under a \
                     different name — GA4 matches names exactly, so `Purchase` and \
                     `purchase` are two unrelated events and only one of them counts."
                ),
                evidence: Some("purchase, generate_lead".into()),
            },
            Finding {
                check: "revenue_tagged",
                grade: Grade::Critical,
                headline: "purchases arrive without their money".into(),
                detail: "`purchase` is firing and the property has recorded no revenue at \
                         all, which happens when the event is sent without its `value` and \
                         `currency` parameters. Every revenue, ARPU and ROAS figure here is \
                         zero as a result."
                    .into(),
                evidence: Some("412 purchases · 0 revenue".into()),
            },
            Finding {
                check: "payment_handoff",
                grade: Grade::Warning,
                headline: "a payment page is credited with conversions".into(),
                detail: "a visitor sent out to pay or sign in and returned starts a new \
                         session referred by the gateway, and whatever actually brought them \
                         loses the conversion to it. List these under Admin → Data Streams → \
                         Configure tag settings → List unwanted referrals."
                    .into(),
                evidence: Some("checkout.stripe.com".into()),
            },
            Finding {
                check: "direct_share",
                grade: Grade::Note,
                headline: "almost everything arrives as direct".into(),
                detail: "direct is where GA4 puts a session with no referrer to read. A share \
                         this high is normal for a site people reach by typing its name, and \
                         otherwise points at campaigns going out without UTM tags."
                    .into(),
                evidence: Some("74% of sessions".into()),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(check: &'static str, grade: Grade) -> Finding {
        Finding {
            check,
            grade,
            headline: "something".into(),
            detail: "because".into(),
            evidence: None,
        }
    }

    fn audit(findings: Vec<Finding>) -> Audit {
        Audit {
            property: "397412345".to_string(),
            title: "Contoso Labs".to_string(),
            days: 28,
            checks: CHECKS.len(),
            findings,
        }
        .sorted()
    }

    #[test]
    fn the_worst_thing_found_comes_first() {
        let report = audit(vec![
            finding("direct_share", Grade::Note),
            finding("event_names", Grade::Warning),
            finding("key_events_firing", Grade::Critical),
        ]);
        let order: Vec<&str> = report.findings.iter().map(|f| f.check).collect();
        assert_eq!(order, ["key_events_firing", "event_names", "direct_share"]);
        assert_eq!(report.worst(), Some(Grade::Critical));
    }

    #[test]
    fn a_clean_pass_still_says_how_hard_it_looked() {
        // "No findings" means nothing on its own — it is only worth something
        // beside the number of ways it tried to find one.
        let report = audit(Vec::new());
        assert!(report.clean());
        let line = summary(&report);
        assert!(
            line.contains(&format!("{} checks run", CHECKS.len())),
            "{line}"
        );
        assert!(line.contains("nothing found"), "{line}");
    }

    #[test]
    fn checks_that_could_not_run_are_counted_as_not_run() {
        let mut report = audit(vec![finding("collecting", Grade::Critical)]);
        report.checks = 3;
        let line = summary(&report);
        assert!(line.contains("3 checks run"), "{line}");
        assert!(
            line.contains(&format!("{} not run", CHECKS.len() - 3)),
            "{line}"
        );
    }

    #[test]
    fn a_host_is_compared_without_its_scheme_port_or_www() {
        assert_eq!(
            host_of("https://www.Example.com/pricing").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_of("example.com").as_deref(), Some("example.com"));
        assert_eq!(
            host_of("http://shop.example.com:8080").as_deref(),
            Some("shop.example.com")
        );
        // sessionSource mixes hosts with channel words; only the hosts can be
        // matched against a domain.
        assert_eq!(host_of("google"), None);
        assert_eq!(host_of("(direct)"), None);
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn a_subdomain_is_under_its_domain_and_a_lookalike_is_not() {
        assert!(under("checkout.stripe.com", "stripe.com"));
        assert!(under("stripe.com", "stripe.com"));
        // The bug this guards: a suffix match without the label boundary
        // reports notstripe.com as a payment gateway.
        assert!(!under("notstripe.com", "stripe.com"));
        assert!(!under("stripe.com.evil.test", "stripe.com"));
    }

    #[test]
    fn names_that_differ_only_in_case_or_punctuation_collide() {
        let counts: BTreeMap<String, f64> = [
            ("sign_up", 100.0),
            ("signUp", 4.0),
            ("add-to-cart", 50.0),
            ("add_to_cart", 9.0),
            ("purchase", 12.0),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

        let found = collisions(&counts);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            found.contains(&"add-to-cart / add_to_cart".to_string()),
            "{found:?}"
        );
        assert!(found.contains(&"sign_up / signUp".to_string()), "{found:?}");
    }

    #[test]
    fn a_longer_name_is_not_a_collision_with_a_shorter_one() {
        // sign_up and sign_up_failed are two events on purpose, and an audit
        // that calls them a defect is an audit nobody finishes reading.
        let counts: BTreeMap<String, f64> = [("sign_up", 100.0), ("sign_up_failed", 12.0)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        assert!(collisions(&counts).is_empty());
    }

    #[test]
    fn a_listing_stops_naming_and_starts_counting() {
        assert_eq!(listing(&["a", "b"]), "a, b");
        let many = ["a", "b", "c", "d", "e", "f"];
        assert_eq!(listing(&many), "a, b, c, d, and 2 more");
    }

    #[test]
    fn wrapping_counts_characters_rather_than_bytes() {
        let lines = wrap("⛏ ⛏ ⛏ ⛏ ⛏", 3);
        assert!(lines.iter().all(|l| l.chars().count() <= 3), "{lines:?}");
        assert_eq!(lines.join(" "), "⛏ ⛏ ⛏ ⛏ ⛏");
    }

    #[test]
    fn json_carries_the_slugs_a_script_matches_on() {
        let report = audit(vec![
            finding("key_events_configured", Grade::Critical),
            finding("direct_share", Grade::Note),
        ]);
        let payload = as_json(&report);

        assert_eq!(payload["clean"], false);
        assert_eq!(payload["counts"]["critical"], 1);
        assert_eq!(payload["counts"]["note"], 1);
        assert_eq!(payload["checks_available"], CHECKS.len());
        assert_eq!(payload["findings"][0]["check"], "key_events_configured");
        assert_eq!(payload["findings"][0]["grade"], "critical");
        assert!(payload["url"].as_str().unwrap().contains("397412345"));
    }

    #[test]
    fn slack_colors_the_bar_by_the_worst_finding() {
        let bad = as_slack(&audit(vec![
            finding("event_names", Grade::Warning),
            finding("revenue_tagged", Grade::Critical),
        ]));
        assert_eq!(bad["attachments"][0]["color"], crate::watch::BAR_ALARM);

        // A clean audit still posts. It is a thing somebody asked for, unlike a
        // quiet watch, and "nothing found" is the answer they asked for.
        let clean = as_slack(&audit(Vec::new()));
        assert_eq!(clean["attachments"][0]["color"], BAR_CLEAR);
        assert!(clean["text"].as_str().unwrap().contains("audit clean"));
    }

    #[test]
    fn slack_carries_a_top_level_text_for_mobile_notifications() {
        // Slack builds a desktop notification out of the blocks but a mobile
        // one out of `text` alone — without it the phone buzzes empty.
        let payload = as_slack(&audit(vec![finding("collecting", Grade::Critical)]));
        assert!(!payload["text"].as_str().unwrap_or_default().is_empty());
    }

    #[test]
    fn the_demo_shows_every_grade_it_can_report() {
        let report = demo_audit(28).sorted();
        for grade in [Grade::Critical, Grade::Warning, Grade::Note] {
            assert!(report.count(grade) > 0, "{grade:?} missing from the demo");
        }
        // And it renders — the panel is the thing `--demo` exists to show.
        let drawn = panels(&report);
        assert!(drawn.contains("AUDIT"), "{drawn}");
        assert!(
            drawn.contains("PURCHASES ARRIVE WITHOUT THEIR MONEY"),
            "{drawn}"
        );
    }

    #[test]
    fn every_slug_that_is_used_is_a_slug_in_the_table() {
        // Both numbers in the summary line are counted against CHECKS: a check
        // that fires under a slug the table does not know about is reported out
        // of a total it was never part of, and one that is skipped under such a
        // slug subtracts from a total it never added to.
        for slug in NEEDS_DATA {
            assert!(
                CHECKS.contains(slug),
                "`{slug}` is skipped on an empty window but is not in CHECKS"
            );
        }

        let source = include_str!("audit.rs");
        let mut seen = 0;
        for line in source.lines().map(str::trim_start) {
            // `check: "x"` is a finding being raised; `skipped.push("x")` is
            // one that could not run. Every other list of slugs in here is a
            // const the assertion above covers directly.
            let used = line
                .strip_prefix("check: \"")
                .or_else(|| line.strip_prefix("skipped.push(\""));
            let Some(rest) = used else { continue };
            let Some(slug) = rest.split('"').next() else {
                continue;
            };
            assert!(
                CHECKS.contains(&slug),
                "`{slug}` is used as a check name but is not in CHECKS"
            );
            seen += 1;
        }
        assert!(seen >= CHECKS.len(), "only matched {seen} slugs");
    }

    #[test]
    fn the_check_table_has_no_repeats() {
        // CHECKS.len() is what a report is counted out of, so a slug listed
        // twice quietly inflates the denominator on every audit.
        let mut sorted = CHECKS.to_vec();
        sorted.sort_unstable();
        let mut unique = sorted.clone();
        unique.dedup();
        assert_eq!(sorted, unique, "a check slug is listed twice");
    }
}
