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
    "measurement_off",
    "measurement_silent",
    "key_events_configured",
    "key_events_unmarked",
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
    "measurement_silent",
    "key_events_unmarked",
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

/// The stream's automatic measurement, as a table: the snake_case field an
/// update mask names it by, how it reads in a sentence, the events it produces
/// when it is on, and whether `--fix` may switch it on unasked.
///
/// Both halves of the pair of checks below are this table read in opposite
/// directions. On with nothing arriving is a contradiction the property is
/// stating about itself; off is simply a thing not being collected, which is
/// sometimes a decision and never a defect.
///
/// That last column is the one worth explaining. Site search and form
/// interactions record what a visitor typed — the query, and which fields of a
/// form were touched — and whether a site collects that is a question about
/// its privacy policy, not about whether its analytics are set up correctly.
/// Somebody running `--fix` to mark a key event has not agreed to start
/// collecting typed input, and a flag that treated the second as implied by
/// the first would be the kind of thing that makes `--fix` unsafe to run. Both
/// are still reported; neither is ever written from here.
const MEASURED: &[(&str, &str, &[&str], bool)] = &[
    ("scrolls_enabled", "scrolls", &["scroll"], true),
    (
        "outbound_clicks_enabled",
        "outbound clicks",
        &["click"],
        true,
    ),
    (
        "site_search_enabled",
        "site search",
        &["view_search_results"],
        false,
    ),
    (
        "video_engagement_enabled",
        "video engagement",
        &["video_start", "video_progress", "video_complete"],
        true,
    ),
    (
        "file_downloads_enabled",
        "file downloads",
        &["file_download"],
        true,
    ),
    (
        "form_interactions_enabled",
        "form interactions",
        &["form_start", "form_submit"],
        false,
    ),
];

/// Events that are an outcome wherever they appear, from GA4's own recommended
/// event tables. Deliberately short, and deliberately not the whole of those
/// tables: `add_to_cart` and `begin_checkout` are steps towards an outcome,
/// and a property that marks them as key events reports a conversion rate that
/// counts the same visitor three times.
///
/// Nothing here is inferred. `craft audit --fix` writes key events from this
/// list and no other, so what it can mark is a list somebody can read in the
/// source before they run it.
const OUTCOMES: &[&str] = &[
    "purchase",
    "generate_lead",
    "sign_up",
    "subscribe",
    "start_trial",
    "contact",
    "submit_lead_form",
    "qualify_lead",
    "close_convert_lead",
];

/// GA4's own ceiling on key events per property. A fix that would cross it is
/// not attempted: the API would reject it, and half-marking a set of outcomes
/// is worse than marking none and saying why.
const KEY_EVENT_CAP: usize = 30;

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

/// A repair `craft audit --fix` can make to the property itself.
///
/// Kept beside the findings rather than inside them, because the two are
/// different things: a finding is what is true, and a fix is what this binary
/// is willing to do about it. Most findings have no fix and never will —
/// an event that is not being sent cannot be made to arrive by an API call,
/// and a report that implied otherwise would be selling a button that does
/// nothing.
///
/// Everything in here is configuration, additive, and undone from the GA4
/// console in one click. Nothing in here turns collection off, lowers
/// retention, edits an event on its way in, or touches a setting the plan
/// printed above it did not name.
pub struct Fix {
    /// The check this repairs, so the report can print it under that finding.
    check: &'static str,
    /// What it will do, in one line, in the same voice as the findings.
    label: String,
    action: Action,
}

enum Action {
    /// Tell GA4 that events already arriving are outcomes.
    MarkKeyEvents(Vec<String>),
    /// Turn stream measurement toggles on, by update-mask field name.
    Measure {
        stream: String,
        fields: Vec<&'static str>,
    },
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
    /// What `--fix` would do, in the order it would do it. Empty on every
    /// property whose problems are all on the site rather than in the console.
    plan: Vec<Fix>,
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

    /// The fix for a given check, if this audit found one.
    fn fix_for(&self, check: &str) -> Option<&Fix> {
        self.plan.iter().find(|f| f.check == check)
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
    let mut plan: Vec<Fix> = Vec::new();
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

    // --- measurement_off and measurement_silent ---------------------------
    //
    // The only place GA4 states an expectation about events rather than
    // counting them, which is what makes this pair worth the extra round trip:
    // every other check compares a number against a threshold somebody here
    // chose, and these two compare the property against what the property was
    // told to do.
    //
    // Read per stream and sequentially. A property has one web stream in the
    // overwhelming case, and the one that has forty is the one where forty
    // concurrent alpha calls is the wrong thing to do to somebody's quota.
    let mut settings: Vec<(&WebStream, crate::ga::EnhancedMeasurement)> = Vec::new();
    let mut unreadable = streams.is_err();
    for stream in streams.iter().flatten() {
        match ga.enhanced_measurement(&stream.name).await {
            Ok(found) => settings.push((stream, found)),
            // v1alpha, and so allowed to vanish — see `ADMIN_ALPHA`. A check
            // that could not be read is not a check that passed.
            Err(_) => unreadable = true,
        }
    }
    if unreadable && settings.is_empty() {
        skipped.push("measurement_off");
        skipped.push("measurement_silent");
    } else {
        // --- measurement_off ----------------------------------------------
        for (stream, found) in &settings {
            if !found.stream_enabled {
                findings.push(Finding {
                    check: "measurement_off",
                    grade: Grade::Warning,
                    headline: "enhanced measurement is switched off".into(),
                    detail: "the stream's automatic events — scrolls, outbound clicks, site \
                             search, video, downloads, form interactions — are configured and \
                             not collected, because the master switch above them is off. \
                             Nothing in the console says so on the reports that are missing \
                             them; the settings underneath keep showing as on."
                        .into(),
                    evidence: Some(stream_label(stream)),
                });
                plan.push(Fix {
                    check: "measurement_off",
                    label: format!("turn enhanced measurement on for {}", stream.measurement_id),
                    action: Action::Measure {
                        stream: stream.name.clone(),
                        fields: vec!["stream_enabled"],
                    },
                });
                break;
            }
        }

        // Individual toggles, and only on streams whose master switch is on —
        // otherwise every stream above would report six more findings saying
        // the same thing the master switch already said.
        if !findings.iter().any(|f| f.check == "measurement_off") {
            for (stream, found) in &settings {
                let off: Vec<&(&str, &str, &[&str], bool)> = MEASURED
                    .iter()
                    .filter(|(field, _, _, _)| !found.on(field))
                    .collect();
                if off.is_empty() {
                    continue;
                }
                let names: Vec<&str> = off.iter().map(|(_, label, _, _)| *label).collect();
                // Reported whole, written in part. See `MEASURED`.
                let writable: Vec<&'static str> = off
                    .iter()
                    .filter(|(_, _, _, fixable)| *fixable)
                    .map(|(field, _, _, _)| *field)
                    .collect();
                let offered: Vec<&str> = off
                    .iter()
                    .filter(|(_, _, _, fixable)| *fixable)
                    .map(|(_, label, _, _)| *label)
                    .collect();
                let withheld: Vec<&str> = off
                    .iter()
                    .filter(|(_, _, _, fixable)| !*fixable)
                    .map(|(_, label, _, _)| *label)
                    .collect();
                findings.push(Finding {
                    check: "measurement_off",
                    grade: Grade::Note,
                    headline: "the stream is not measuring everything it could".into(),
                    detail: "these are collected by the tag already on the site, with no \
                             code to write and nothing to deploy — turning one on starts it \
                             reporting from that moment. A note rather than a warning \
                             because some of them are off on purpose: form interactions and \
                             site search both record what a visitor typed, which is a \
                             decision about a privacy policy rather than an oversight."
                        .into(),
                    evidence: Some(listing(&names)),
                });
                if !writable.is_empty() {
                    plan.push(Fix {
                        check: "measurement_off",
                        label: format!(
                            "turn on {} for {}{}",
                            listing(&offered),
                            stream.measurement_id,
                            if withheld.is_empty() {
                                String::new()
                            } else {
                                // Named rather than quietly dropped: a plan
                                // that lists four of six and says nothing
                                // about the other two reads as one that covers
                                // everything.
                                format!(
                                    " · {} stay off — those are a console decision",
                                    listing(&withheld)
                                )
                            },
                        ),
                        action: Action::Measure {
                            stream: stream.name.clone(),
                            fields: writable,
                        },
                    });
                }
                break;
            }
        }

        // --- measurement_silent -------------------------------------------
        //
        // The contradiction, and the reason the alpha read is worth making. A
        // toggle that is on is the property saying this event will arrive; a
        // count of zero is it not having arrived for a month. Unlike every
        // other threshold here there is nothing to tune — the expectation is
        // Google's, not ours.
        if dark || sessions < MIN_SESSIONS {
            skipped.push("measurement_silent");
        } else {
            let measured: Vec<&crate::ga::EnhancedMeasurement> =
                settings.iter().map(|(_, found)| found).collect();
            let silent = silent_measurement(&measured, &counts);
            if !silent.is_empty() {
                findings.push(Finding {
                    check: "measurement_silent",
                    grade: Grade::Warning,
                    headline: "measurement is on and nothing arrives".into(),
                    detail: format!(
                        "the stream is configured to collect these automatically and has \
                         not recorded one of them in {days} days. Enhanced measurement \
                         works by watching the page, so it goes quiet when there is nothing \
                         of that shape to watch — a single-page app that never fires a real \
                         navigation, a video embedded without the parameter that lets GA4 \
                         read it, a search page whose query is in the path rather than a \
                         parameter. The setting stays on and green throughout."
                    ),
                    evidence: Some(listing(&silent)),
                });
            }
        }
    }

    // --- key_events_configured --------------------------------------------
    //
    // The one that pays for the command. A property can collect flawlessly for
    // a year and still answer no question anybody has, because nothing in it
    // was ever marked as the point.
    // Outcomes that are arriving and are not marked. The same list answers
    // both of the checks below, and is what `--fix` writes: an event nobody
    // has to be asked about, because the property is already recording it.
    let unmarked: Vec<String> = match (&keys, dark) {
        (Ok(list), false) => unmarked_outcomes(&counts, list),
        _ => Vec::new(),
    };
    // GA4 rejects the thirty-first, and a fix that marked four of six outcomes
    // and failed on the rest would leave the property in a state nobody asked
    // for. Offered whole or not at all.
    let room = keys
        .as_ref()
        .map(|list| list.len() + unmarked.len() <= KEY_EVENT_CAP)
        .unwrap_or(false);

    match &keys {
        Err(_) => {
            skipped.push("key_events_configured");
            skipped.push("key_events_unmarked");
            skipped.push("key_events_firing");
        }
        Ok(list) if list.is_empty() => {
            findings.push(Finding {
                check: "key_events_configured",
                grade: Grade::Critical,
                headline: "nothing is marked as a key event".into(),
                detail: "GA4 is counting traffic and nothing else. With no key event, no \
                         report on this property can say whether any of that traffic was \
                         worth having, and Google Ads has nothing to import or optimise \
                         against. Mark the events that matter under Admin → Events."
                    .into(),
                evidence: None,
            });
            // The property is already recording outcomes and has never been
            // told they are outcomes — which makes this the one finding here
            // that is fixed entirely from the console side.
            if !unmarked.is_empty() && room {
                plan.push(Fix {
                    check: "key_events_configured",
                    label: format!(
                        "mark {} as {}",
                        listing(&unmarked.iter().map(String::as_str).collect::<Vec<_>>()),
                        plural(unmarked.len(), "a key event", "key events"),
                    ),
                    action: Action::MarkKeyEvents(unmarked.clone()),
                });
            }
            // `key_events_unmarked` has nothing left to say once the finding
            // above has said it louder.
            skipped.push("key_events_unmarked");
        }
        Ok(_) => {}
    }

    // --- key_events_unmarked ----------------------------------------------
    //
    // The quieter half. A property with key events already set up is not
    // broken, but an outcome arriving thousands of times that nobody marked is
    // a report that exists and is not being read.
    if !skipped.contains(&"key_events_unmarked") && !unmarked.is_empty() {
        let shown: Vec<String> = unmarked
            .iter()
            .map(|name| {
                format!(
                    "{name} {}",
                    render::commas(counts.get(name).copied().unwrap_or(0.0))
                )
            })
            .collect();
        findings.push(Finding {
            check: "key_events_unmarked",
            grade: Grade::Warning,
            headline: format!(
                "{} arrives unmarked",
                plural(unmarked.len(), "an outcome", "outcomes")
            ),
            detail: "these are events GA4's own recommended set treats as outcomes, they \
                     are firing on this property, and none of them is marked as a key \
                     event. Nothing is lost — the events are collected and the history is \
                     there the moment one is marked — but until then no conversion report \
                     counts them and Google Ads cannot import them."
                .into(),
            evidence: Some(listing(
                &shown.iter().map(String::as_str).collect::<Vec<_>>(),
            )),
        });
        if room {
            plan.push(Fix {
                check: "key_events_unmarked",
                label: format!(
                    "mark {} as {}",
                    listing(&unmarked.iter().map(String::as_str).collect::<Vec<_>>()),
                    plural(unmarked.len(), "a key event", "key events"),
                ),
                action: Action::MarkKeyEvents(unmarked.clone()),
            });
        }
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
        plan,
    }
    .sorted())
}

// ------------------------------------------------------------------ helpers ---

/// Outcomes that are arriving and are not marked as key events.
///
/// Pure, and separate from the check that reports it, because this is the list
/// `--fix` writes to somebody's property — the thing worth being able to state
/// the behaviour of in a test rather than inferring it from a report.
fn unmarked_outcomes(counts: &BTreeMap<String, f64>, keys: &[KeyEvent]) -> Vec<String> {
    OUTCOMES
        .iter()
        // Arriving. An outcome a site does not have is not a finding, and
        // marking an event that has never fired creates a key event whose
        // report is permanently empty.
        .filter(|name| counts.get(**name).copied().unwrap_or(0.0) > 0.0)
        // And not already the point. Exact match, because GA4's is: a property
        // with `Purchase` marked has not marked `purchase`.
        .filter(|name| !keys.iter().any(|k| k.name == **name))
        .map(|name| name.to_string())
        .collect()
}

/// Measurement that is switched on and has recorded nothing.
///
/// Named once per stream at most, and only for streams whose master switch is
/// on — a stream with enhanced measurement off is not silent, it is off, and
/// `measurement_off` has already said so.
fn silent_measurement(
    settings: &[&crate::ga::EnhancedMeasurement],
    counts: &BTreeMap<String, f64>,
) -> Vec<&'static str> {
    let mut silent: Vec<&'static str> = Vec::new();
    for found in settings {
        if !found.stream_enabled {
            continue;
        }
        for (field, label, events, _) in MEASURED {
            if !found.on(field) || silent.contains(label) {
                continue;
            }
            let arrived = events
                .iter()
                .any(|e| counts.get(*e).copied().unwrap_or(0.0) > 0.0);
            if !arrived {
                silent.push(label);
            }
        }
    }
    silent
}

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
        // Under the detail rather than beside the headline: the finding is
        // what is true and the fix is an offer, and a reader who disagrees
        // with the first should not have already read a button.
        if let Some(fix) = audit.fix_for(finding.check) {
            for (n, line) in wrap(&format!("fix · {}", fix.label), TEXT_WIDTH)
                .into_iter()
                .enumerate()
            {
                let text = if n == 0 { line } else { format!("  {line}") };
                out.push_str(&format!("    {}\n", paint(&text, ore::emerald())));
            }
        }
        out.push('\n');
    }

    if !audit.plan.is_empty() {
        out.push_str(&format!(
            "  {}\n",
            dim(&format!(
                "{} of these can be fixed from here · craft audit --fix",
                audit.plan.len(),
            ))
        ));
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
        "fixable": audit.plan.len(),
        "findings": audit.findings.iter().map(|f| json!({
            "check": f.check,
            "grade": f.grade.slug(),
            "headline": f.headline,
            "detail": f.detail,
            "evidence": f.evidence,
            // Null on most of them, and that is the point: a script can tell
            // which findings `--fix` would touch without parsing English.
            "fix": audit.fix_for(f.check).map(|fix| fix.label.clone()),
        })).collect::<Vec<_>>(),
    })
}

/// One object, shaped the way `overview --format json` and `watch --format
/// json` are: the findings, plus which property and when.
fn as_json(audit: &Audit, repaired: &[&str]) -> Value {
    let mut payload = findings_payload(audit);
    if let Some(object) = payload.as_object_mut() {
        // Always present, empty without `--fix`, so a script reads one shape.
        object.insert("applied".into(), json!(repaired));
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
/// thing somebody asked for, and "fifteen checks, nothing found" is the answer
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

/// Findings still standing once `--fix` has done what it can.
fn outstanding(audit: &Audit, repaired: &[&str]) -> usize {
    audit
        .findings
        .iter()
        .filter(|f| !repaired.contains(&f.check))
        .count()
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
    /// Apply the plan the report prints, instead of only printing it.
    pub fix: bool,
}

/// Apply the plan, and say what happened to each part of it.
///
/// Sequential, and one line printed per write as it lands rather than a
/// summary at the end. This is changing somebody's production property: if the
/// third of five writes fails, the two that already succeeded are on the
/// screen and not inside a spinner that got replaced by an error.
///
/// A failure does not stop the rest. Each of these is independent — marking
/// `sign_up` has nothing to do with turning on file downloads — so stopping at
/// the first would leave a property half-calibrated for no reason.
/// `quiet` is for the formats that are a pipe rather than a screen. A panel
/// printed after a JSON object is the kind of thing that works when a person
/// runs it and breaks the first time it is put in a cron line, so under
/// `--format json` nothing is printed here and what was repaired travels in
/// the object instead.
async fn apply(ga: &Ga, property: &str, audit: &Audit, quiet: bool) -> Result<Vec<&'static str>> {
    let why = format!(
        "fixing {} needs permission to change your Analytics settings",
        audit.title,
    );
    ga.auth()
        .ensure_scope(crate::auth::SCOPE_EDIT, &why)
        .await?
        .show(&crate::auth::GRANTED);

    println!(
        "\n{}\n",
        panel_top(&format!("FIX · {}", audit.title.to_uppercase()))
    );

    let mut repaired = Vec::new();
    let mut denied = false;

    for fix in &audit.plan {
        let outcome = match &fix.action {
            Action::MarkKeyEvents(names) => {
                let mut failed = None;
                let mut marked = 0;
                for name in names {
                    match ga.create_key_event(property, name).await {
                        Ok(()) => marked += 1,
                        Err(e) => {
                            failed = Some(e);
                            break;
                        }
                    }
                }
                match failed {
                    // Partial is reported as partial. The cap check in
                    // `examine` is what should have prevented this, and if it
                    // did not then the count that got through is the one thing
                    // worth knowing.
                    Some(e) if marked > 0 => Err(anyhow::anyhow!(
                        "{e}\n  {marked} of {} were marked before this",
                        names.len()
                    )),
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            }
            // Read again immediately before writing. The audit above may be
            // seconds or minutes old, and the write has to hand back the
            // property's own site-search parameters rather than a guess at
            // them — so the value that goes up is the one that is up there
            // now, not the one that was when the report printed.
            Action::Measure { stream, fields } => match ga.enhanced_measurement(stream).await {
                Ok(current) => {
                    ga.enable_measurement(stream, &current, &fields.to_vec())
                        .await
                }
                Err(e) => Err(e),
            },
        };

        match outcome {
            Ok(()) => {
                if !quiet {
                    println!(
                        "  {} {}",
                        paint(&glyph::FULL.to_string(), ore::emerald()),
                        paint(&fix.label, ore::emerald()),
                    );
                }
                repaired.push(fix.check);
            }
            Err(e) => {
                denied |= format!("{e}").contains("access denied");
                if !quiet {
                    println!(
                        "  {} {}",
                        paint(&glyph::PARTIAL.to_string(), ore::redstone()),
                        paint(&fix.label, ore::redstone()),
                    );
                    for line in wrap(&format!("{e}"), TEXT_WIDTH) {
                        println!("    {}", dim(&line));
                    }
                }
            }
        }
    }

    if quiet {
        return Ok(repaired);
    }

    if denied {
        println!();
        for line in wrap(
            "changing settings needs Editor or Administrator on the property. Viewer is \
             enough to run the audit and not enough to fix it — the account that owns the \
             property grants the role under Admin → Property access management.",
            TEXT_WIDTH,
        ) {
            println!("  {}", dim(&line));
        }
    }

    println!();
    for line in wrap(
        "these are settings, not data. Nothing here changes what was collected before \
         now, and everything here is undone from the GA4 console — key events under \
         Admin → Events, measurement under Admin → Data Streams.",
        TEXT_WIDTH,
    ) {
        println!("  {}", dim(&line));
    }
    println!("{}\n", panel_bottom());

    Ok(repaired)
}

/// Audit once and exit.
pub async fn run(cfg: &Config, property: Option<&str>, opts: Options) -> Result<()> {
    let days = opts.days.max(1);

    if opts.demo {
        // The shop window, same as everywhere else: what the report looks like,
        // before anything is paid for or connected.
        let audit = demo_audit(days).sorted();
        emit(&audit, opts.format, &[]);
        if opts.fix {
            // The demo property is a literal in this file. Saying so beats
            // printing a plan that appears to have been applied to something.
            println!(
                "  {}\n",
                dim("--fix has nothing to write to on --demo: this property is synthetic."),
            );
        }
        return finish(&audit, &[]);
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

    let ga = Ga::new()?;
    let audit = examine(&ga, &id, &title, days).await?;

    // Report first, then repair. The order is the whole argument for letting a
    // command that reads also write: nobody is asked to trust a fix they have
    // not been shown the reason for, and a `--fix` run is a `craft audit` run
    // with the same output plus what it did about it.
    //
    // For a person. The other two formats are one object on stdout, and a
    // second one after it is not something a pipe can read — so those repair
    // first and report once, with what was repaired inside the object.
    let to_a_screen = matches!(opts.format, Format::Panels);
    if to_a_screen {
        emit(&audit, opts.format, &[]);
    }

    let repaired = if opts.fix && !audit.plan.is_empty() {
        apply(&ga, &id, &audit, !to_a_screen).await?
    } else {
        Vec::new()
    };

    if !to_a_screen {
        emit(&audit, opts.format, &repaired);
    }

    finish(&audit, &repaired)
}

fn emit(audit: &Audit, format: Format, repaired: &[&str]) {
    match format {
        // The panel says what was repaired in its own section, printed as each
        // write lands rather than after all of them.
        Format::Panels => print!("{}", panels(audit)),
        Format::Json => println!("{}", as_json(audit, repaired)),
        Format::Slack => println!("{}", as_slack(audit)),
    }
}

/// `0` when the property is clean, `2` when it is not, so a shell can gate on
/// it the way it gates on `craft watch`.
///
/// A finding that `--fix` just repaired does not hold the exit code open. The
/// alternative is a `--fix` run that always exits 2 on a property it has
/// finished fixing, which makes the flag unusable from the CI job that is the
/// reason to have it.
fn finish(audit: &Audit, repaired: &[&str]) -> Result<()> {
    if outstanding(audit, repaired) == 0 {
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
                check: "key_events_unmarked",
                grade: Grade::Warning,
                headline: "outcomes arrive unmarked".into(),
                detail: "these are events GA4's own recommended set treats as outcomes, they \
                         are firing on this property, and none of them is marked as a key \
                         event. Until one is, no conversion report counts them and Google \
                         Ads cannot import them."
                    .into(),
                evidence: Some("sign_up 1,204 · subscribe 318".into()),
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
        // One of the four is fixable from here, which is the honest ratio and
        // the reason the demo shows it: the rest are on the site.
        plan: vec![Fix {
            check: "key_events_unmarked",
            label: "mark sign_up and subscribe as key events".into(),
            action: Action::MarkKeyEvents(vec!["sign_up".into(), "subscribe".into()]),
        }],
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
            plan: Vec::new(),
        }
        .sorted()
    }

    fn key(name: &str) -> KeyEvent {
        KeyEvent {
            name: name.to_string(),
            custom: true,
        }
    }

    fn counted(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs
            .iter()
            .map(|(name, n)| (name.to_string(), *n))
            .collect()
    }

    fn measuring(on: bool) -> crate::ga::EnhancedMeasurement {
        crate::ga::EnhancedMeasurement {
            stream_enabled: true,
            scrolls: on,
            outbound_clicks: on,
            site_search: on,
            video_engagement: on,
            file_downloads: on,
            form_interactions: on,
            search_query_parameter: "q".into(),
        }
    }

    #[test]
    fn an_outcome_that_is_arriving_and_unmarked_is_the_one_offered() {
        let counts = counted(&[
            ("purchase", 412.0),
            ("sign_up", 90.0),
            ("page_view", 90_000.0),
        ]);
        let marked = unmarked_outcomes(&counts, &[key("purchase")]);

        // `purchase` is already the point, `page_view` is not an outcome, and
        // `sign_up` is the only thing left to offer.
        assert_eq!(marked, ["sign_up"]);
    }

    #[test]
    fn an_outcome_that_has_never_fired_is_not_offered() {
        // Marking it would create a key event whose report is empty for as
        // long as the property exists — the opposite of calibration.
        let counts = counted(&[("purchase", 0.0), ("subscribe", 3.0)]);
        assert_eq!(unmarked_outcomes(&counts, &[]), ["subscribe"]);
    }

    #[test]
    fn a_name_that_differs_in_case_is_not_the_event_that_was_marked() {
        // GA4 matches exactly, so a property with `Purchase` marked has not
        // marked `purchase`, and saying otherwise would leave it uncounted.
        let counts = counted(&[("purchase", 412.0)]);
        assert_eq!(unmarked_outcomes(&counts, &[key("Purchase")]), ["purchase"]);
    }

    #[test]
    fn measurement_that_is_on_and_recorded_nothing_is_the_contradiction() {
        let counts = counted(&[("scroll", 12_000.0), ("click", 300.0)]);
        let silent = silent_measurement(&[&measuring(true)], &counts);

        // Scrolls and outbound clicks arrived; the other four are on and have
        // nothing to show for a month.
        assert_eq!(
            silent,
            [
                "site search",
                "video engagement",
                "file downloads",
                "form interactions"
            ]
        );
    }

    #[test]
    fn measurement_that_is_off_is_not_reported_as_silent() {
        // Off and quiet is not a contradiction, it is a setting. Reporting it
        // here would say the same thing `measurement_off` already said, in
        // language that implies something is broken.
        let silent = silent_measurement(&[&measuring(false)], &counted(&[]));
        assert!(silent.is_empty(), "got {silent:?}");
    }

    #[test]
    fn a_stream_switched_off_entirely_has_nothing_to_be_silent_about() {
        let mut settings = measuring(true);
        settings.stream_enabled = false;
        let silent = silent_measurement(&[&settings], &counted(&[]));
        assert!(silent.is_empty(), "got {silent:?}");
    }

    #[test]
    fn two_streams_missing_the_same_thing_say_it_once() {
        let counts = counted(&[
            ("scroll", 1.0),
            ("click", 1.0),
            ("view_search_results", 1.0),
        ]);
        let silent = silent_measurement(&[&measuring(true), &measuring(true)], &counts);
        assert_eq!(
            silent,
            ["video engagement", "file downloads", "form interactions"]
        );
    }

    #[test]
    fn a_fix_prints_under_the_finding_it_repairs_and_nowhere_else() {
        let named = |check: &'static str, headline: &str| Finding {
            check,
            grade: Grade::Warning,
            headline: headline.into(),
            detail: "because".into(),
            evidence: None,
        };
        let report = Audit {
            property: "397412345".to_string(),
            title: "Contoso Labs".to_string(),
            days: 28,
            checks: CHECKS.len(),
            findings: vec![
                named("key_events_unmarked", "outcomes arrive unmarked"),
                named("double_counting", "page views look counted twice"),
            ],
            plan: vec![Fix {
                check: "key_events_unmarked",
                label: "mark sign_up as a key event".into(),
                action: Action::MarkKeyEvents(vec!["sign_up".into()]),
            }],
        }
        .sorted();

        // Plain here: `paint` is a no-op when stdout is not a terminal, which
        // under `cargo test` it never is.
        let rendered = panels(&report);
        assert_eq!(rendered.matches("fix ·").count(), 1, "{rendered}");

        // Under its own finding: after the headline it repairs, and before the
        // next one, which has no fix and must not look like it does.
        let unmarked = rendered.find("OUTCOMES ARRIVE UNMARKED").unwrap();
        let fix = rendered.find("fix ·").unwrap();
        let counted_twice = rendered.find("PAGE VIEWS LOOK COUNTED TWICE").unwrap();
        assert!(unmarked < fix && fix < counted_twice, "{rendered}");

        assert!(rendered.contains("1 of these can be fixed from here"));
    }

    #[test]
    fn a_report_with_nothing_to_fix_does_not_advertise_the_flag() {
        let rendered = panels(&audit(vec![finding("direct_share", Grade::Note)]));
        assert!(!rendered.contains("--fix"), "{rendered}");
        assert!(!rendered.contains("fix ·"), "{rendered}");
    }

    #[test]
    fn the_json_says_which_findings_a_fix_would_touch() {
        let report = Audit {
            property: "397412345".to_string(),
            title: "Contoso Labs".to_string(),
            days: 28,
            checks: CHECKS.len(),
            findings: vec![
                finding("key_events_unmarked", Grade::Warning),
                finding("double_counting", Grade::Warning),
            ],
            plan: vec![Fix {
                check: "key_events_unmarked",
                label: "mark sign_up as a key event".into(),
                action: Action::MarkKeyEvents(vec!["sign_up".into()]),
            }],
        }
        .sorted();

        let payload = findings_payload(&report);
        assert_eq!(payload["fixable"], json!(1));
        let findings = payload["findings"].as_array().unwrap();
        let fixed = findings
            .iter()
            .find(|f| f["check"] == "key_events_unmarked")
            .unwrap();
        assert_eq!(fixed["fix"], json!("mark sign_up as a key event"));
        // Null rather than absent, so a script can read the field on every
        // finding instead of testing for its existence.
        let site_side = findings
            .iter()
            .find(|f| f["check"] == "double_counting")
            .unwrap();
        assert!(site_side["fix"].is_null());
    }

    #[test]
    fn a_finding_that_was_just_repaired_does_not_hold_the_exit_code_open() {
        let report = audit(vec![
            finding("key_events_unmarked", Grade::Warning),
            finding("double_counting", Grade::Warning),
        ]);
        assert_eq!(outstanding(&report, &["key_events_unmarked"]), 1);
        assert_eq!(
            outstanding(&report, &["key_events_unmarked", "double_counting"]),
            0
        );
        // And a plain `craft audit` still reports everything it found.
        assert_eq!(outstanding(&report, &[]), 2);
    }

    #[test]
    fn every_outcome_the_fix_can_mark_is_an_outcome_and_not_a_step() {
        // The cart and checkout events are the ones somebody reaches for when
        // this list is edited, and marking them turns one visitor into three
        // conversions. Pinned so that edit has to argue with a test.
        for step in ["add_to_cart", "begin_checkout", "view_item", "form_submit"] {
            assert!(
                !OUTCOMES.contains(&step),
                "{step} is a step towards an outcome, not one"
            );
        }
        assert!(OUTCOMES.contains(&"purchase"));
    }

    #[test]
    fn the_measurement_table_names_fields_the_update_mask_understands() {
        // Snake case, and the suffix the API uses — a mask field it does not
        // recognise is a write that fails after the report promised it.
        for (field, label, events, _) in MEASURED {
            assert!(field.ends_with("_enabled"), "{field}");
            assert!(!field.contains(char::is_uppercase), "{field}");
            assert!(!label.is_empty());
            assert!(!events.is_empty(), "{field} produces no event");
        }
    }

    #[test]
    fn the_fix_never_switches_on_collection_of_what_a_visitor_typed() {
        // Somebody running `--fix` to mark a key event has not agreed to start
        // recording search queries and form input. Both are still reported;
        // neither is ever written from here.
        for (field, _, _, fixable) in MEASURED {
            let typed = matches!(*field, "site_search_enabled" | "form_interactions_enabled");
            assert_eq!(
                !typed, *fixable,
                "{field} is on the wrong side of the privacy line"
            );
        }
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
        let payload = as_json(&report, &[]);

        assert_eq!(payload["clean"], false);
        assert_eq!(payload["counts"]["critical"], 1);
        assert_eq!(payload["counts"]["note"], 1);
        assert_eq!(payload["checks_available"], CHECKS.len());
        assert_eq!(payload["findings"][0]["check"], "key_events_configured");
        assert_eq!(payload["findings"][0]["grade"], "critical");
        assert!(payload["url"].as_str().unwrap().contains("397412345"));
        // Present and empty on a plain run, so a script reads one shape
        // whether or not `--fix` was passed.
        assert_eq!(payload["applied"], json!([]));

        let fixed = as_json(&report, &["key_events_configured"]);
        assert_eq!(fixed["applied"], json!(["key_events_configured"]));
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
