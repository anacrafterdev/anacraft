//! The full-screen live dashboard (`anacraft dash`), built on ratatui.
//!
//! Everything on screen moves. Values ease toward their new targets instead of
//! snapping, bars keep a mining edge while they are still filling, the realtime
//! count is polled on its own faster cadence, and arrivals since the last poll
//! scroll past as a feed — so the panel changes between report refreshes rather
//! than sitting frozen for thirty seconds at a time.
//!
//! Network work runs in spawned tasks and lands over a channel; the frame loop
//! never awaits the API, which is what keeps the animation smooth across a slow
//! request.
//!
//! The one-shot commands render ANSI strings directly; those can't be reused
//! here because ratatui needs styled spans, so the block-drawing helpers are
//! reimplemented against `Line`/`Span`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::supports_keyboard_enhancement;
use rand::Rng;
use ratatui::prelude::*;
use ratatui::widgets::{Axis, Block, Borders, Chart, Clear, Dataset, GraphType, Paragraph};

use crate::avatar::{self, Avatar};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::config::{Config, Property};
use crate::ga::{DateRange, Ga, ReportRequest};
use crate::render::{commas, value};
use crate::theme::{self, glyph, ore, OVERVIEW};

/// Frame budget. 20fps is plenty for block glyphs and leaves the CPU idle.
const FRAME: Duration = Duration::from_millis(50);
/// Default realtime polling cadence, deliberately independent of the report
/// refresh — this is the number people actually watch. Overridable with
/// `--live-refresh`; the floor keeps a fast setting from burning realtime
/// quota.
pub const LIVE_EVERY: u64 = 5;
const LIVE_FLOOR: u64 = 2;
/// How fast eased values close on their target. Higher is snappier; this lands
/// a big jump in a bit under a second.
const EASE: f64 = 4.5;
/// How long a row stays highlighted after its value changes.
const FLASH: Duration = Duration::from_millis(1400);
/// Live-feed entries older than this are dropped.
const FEED_TTL: Duration = Duration::from_secs(90);
/// The smallest terminal the dashboard will draw in.
///
/// Width is one readable tile and the border around it: the grid drops to a
/// single stacked column rather than squeezing tiles under `MIN_TILE_COLS`,
/// and the header gives up its day span and its badge to match, so a phone-
/// sized terminal gets the dashboard instead of a notice. It used to be 80 —
/// the width the vitals rows needed before they learned to size their bars to
/// the panel and drop the subtitle — which is why the site's own narrow
/// capture, taken at 74 columns for phones, was an error card.
///
/// Height is what the frame itself costs — a 3-row header, the supporter box,
/// the footer, the gutters between them, and the 10 rows the body is never
/// given less than. Under either, the panels don't degrade so much as
/// disintegrate, so the dashboard says what it needs and waits instead.
const MIN_COLS: u16 = 60;
const MIN_ROWS: u16 = 24;
/// The supporter box: one line and its borders.
const SUPPORTER_ROWS: u16 = 3;
/// Rows the event feed always occupies, lit or not.
///
/// A fixed field, because the panel is a screen: an LCD does not shrink when it
/// has less to say. It also stops the box below it walking up and down the
/// column every time an event ages out, which is the kind of motion a dashboard
/// left open on a second screen should never make.
const FEED_ROWS: usize = 6;
/// What the vitals panel wants: a label, a bar and a gap for each headline
/// metric, the daily sparkline, and its borders.
const VITALS_ROWS: u16 = (OVERVIEW.len() as u16) * 3 + 2;
/// The same panel with the gaps closed up. Denser than it wants to be, and
/// still whole — every metric keeps its bar.
const VITALS_TIGHT_ROWS: u16 = (OVERVIEW.len() as u16) * 2 + 2;
/// The least it can be and still be the vitals: one line of numbers per
/// metric, the bars given up. This is all the column reserves for it, so the
/// map is never the panel that pays for a tall table.
const VITALS_MIN_ROWS: u16 = OVERVIEW.len() as u16 + 2;
/// The daily chart's box: three rows of bars, a caption, and borders.
const TREND_ROWS: u16 = 3 + 1 + 2;
/// What the live panel needs before it is worth drawing: the count, its
/// caption, the meter, the graph and the feed's full field, plus borders.
const LIVE_ROWS: u16 = 8 + FEED_ROWS as u16;
/// Rows the realtime graph gets inside the live panel.
///
/// Three, because that is the slack `LIVE_ROWS` already carried: two header
/// lines, the graph, a blank, `FEED_ROWS` of feed and two borders comes to
/// exactly the budget. Taking more would take it off another panel, and on a
/// short terminal the layout drops whole panels rather than squeezing them.
const LIVE_GRAPH_ROWS: usize = 3;
/// Two chunks and borders. It takes the column's spare rows on top of this.
const CHUNKS_ROWS: u16 = 6;
/// Ranked realms: eight country rows with bars, plus borders.
const REALMS_RANKED_ROWS: u16 = 10;
/// Ranked portals: the same shape, and the same eight rows of it.
const PORTALS_ROWS: u16 = 10;
/// The map's box: nine rows of world, a caption, and borders.
/// The narrowest a tile may be squeezed and still be worth reading. Under it
/// the panel headers drop their figure, the lists cut their labels mid-word
/// and the map loses its caption — so the grid gives up a column before it
/// gives up the reading.
const MIN_TILE_COLS: u16 = 36;

/// The room the map's caption keeps for a realm name before it gives up the
/// online tally behind it.
const CAPTION_NAME: usize = 12;

const MAP_ROWS: u16 = 12;
/// The most rows the map can put to use: the template is `WORLD.len()` rows
/// tall and `map_panel` clamps to it, so every row past this one is drawn as
/// nothing. Worth naming because the column hands its slack to whoever is at
/// the bottom, and for a while that was a box that could not spend it — the
/// dead ground under the caption in a tall terminal was exactly this.
const MAP_MAX_ROWS: u16 = WORLD.len() as u16 + 3;
/// The events chart, in the map's box.
///
/// Deliberately the same height and written as the same number: the two are
/// the only picture panels in the left column, they sit one above the other,
/// and two boxes that are nearly the same size read as a mistake rather than
/// as a choice. Twelve rows is nine of plot once the borders and the axis
/// labels are paid for, which is enough to tell six lines apart — the legend
/// costs nothing, it rides the bottom border.
const EVENTS_ROWS: u16 = MAP_ROWS;
/// Width of the view-count column on the chunk rows.
const VIEWS_COLUMN: usize = 8;
/// Width of the share-of-page-views column beside it.
const SHARE_COLUMN: usize = 5;
/// Width of the "climbed two places" marker on a chunk's heading line.
const MOVED_COLUMN: usize = 4;
/// The vitals name column
const NAME_COLUMN: usize = 14;
/// The number and two spaces that keep it off the delta
const VALUE_COLUMN: usize = 12;
/// The widest delta the row can carry
const DELTA_COLUMN: usize = 6;
/// How often the live graph takes a column. Independent of the poll: the graph
/// scrolls on this clock whether or not a new sample has arrived, which is what
/// keeps the panel moving the way btop's graphs do.
const TRACE_EVERY: Duration = Duration::from_millis(500);
/// Columns of trace kept — two minutes at [`TRACE_EVERY`].
const HISTORY: usize = 240;

// ------------------------------------------------------------- animation ---

/// A scalar that chases its target instead of jumping to it.
#[derive(Clone, Copy)]
struct Eased {
    shown: f64,
    target: f64,
}

impl Eased {
    /// Starts at zero so the first frame after launch grows into place.
    fn new(target: f64) -> Eased {
        Eased { shown: 0.0, target }
    }

    fn to(&mut self, target: f64) {
        self.target = target;
    }

    /// Framerate-independent exponential ease-out: the per-frame step is
    /// derived from elapsed time, so a dropped frame doesn't slow the motion.
    fn step(&mut self, dt: f64) {
        if self.shown == self.target {
            return;
        }
        self.shown += (self.target - self.shown) * (1.0 - (-dt * EASE).exp());
        // Snap once the gap is invisible, otherwise `moving()` never settles
        // and the mining edge flickers forever.
        if (self.target - self.shown).abs() < (self.target.abs() * 1e-3).max(1e-4) {
            self.shown = self.target;
        }
    }

    fn moving(&self) -> bool {
        self.shown != self.target
    }
}

/// Fades from 1 to 0 over `FLASH` after a value changes.
fn flash_level(at: Option<Instant>) -> f64 {
    match at {
        Some(at) => {
            let elapsed = at.elapsed().as_secs_f64();
            let span = FLASH.as_secs_f64();
            if elapsed >= span {
                0.0
            } else {
                1.0 - elapsed / span
            }
        }
        None => 0.0,
    }
}

// ------------------------------------------------------------------ data ---

/// Event counts keyed by GA's `YYYYMMDD`, in chronological order.
type Daily = Vec<(String, f64)>;

/// What the events chart is drawn from.
///
/// The period's daily totals, the same days for the period before it, and the
/// handful of event names the total is mostly made of. The names are the answer
/// to the question the old two-line chart always raised and never answered:
/// events are up, but up in *what*.
struct EventCounts {
    current: Daily,
    previous: Daily,
    /// One series per event name, ranked by its total over the period, each
    /// already laid on the days of `current` — one count per day, zeros
    /// included. Aligned here rather than at draw time because a name with no
    /// row on Tuesday is the normal case, not the exception.
    names: Vec<(String, Vec<f64>)>,
}

/// The most event-name lines the chart will draw.
///
/// Four, because the palette has four ores to spare before two lines start
/// reading as the same colour, and because the breakdown exists to say which
/// handful of events the total is made of rather than to list them all.
const EVENT_NAME_LINES: usize = 4;

/// The ores the name lines are handed, in rank order. Chosen for hue rather
/// than for brightness: these lines are told apart by colour, not by weight.
fn event_ores() -> [Color; EVENT_NAME_LINES] {
    [ore::gold(), ore::emerald(), ore::lapis(), ore::copper()]
}

/// One report pass. The realtime number is not in here: it arrives on its own
/// cadence, and folding it in would make it as stale as the reports.
struct Snapshot {
    current: Vec<f64>,
    previous: Vec<f64>,
    daily: Vec<f64>,
    pages: Vec<(String, f64)>,
    realms: Vec<(String, f64)>,
    /// Where the sessions arrived from, ranked. Source and medium together, so
    /// a link from a search result and an ad on the same site are not one row.
    portals: Vec<(String, f64)>,
    /// Event counts per day, for the period and the one before it.
    events: EventCounts,
}

/// A report arrives in pieces, and each piece is painted the moment it lands
/// rather than being held until the whole set is in — a slow chunk list no
/// longer delays the headline numbers.
enum Update {
    /// Totals and the period they are compared against travel together: a total
    /// without its previous period would render a wrong delta for a frame.
    Totals {
        current: Vec<f64>,
        previous: Vec<f64>,
    },
    Trend(Vec<f64>),
    Pages(Vec<(String, f64)>),
    /// Event counts per day, for the period and the one before it.
    Events(EventCounts),
    /// Users by country for the period — where they came from.
    Realms(Vec<(String, f64)>),
    /// Sessions by source and medium — who is sending them.
    Portals(Vec<(String, f64)>),
    Live {
        total: f64,
        realms: Vec<(String, f64)>,
    },
    Failed(String),
}

struct MetricRow {
    value: Eased,
    frac: Eased,
    previous: f64,
    flash: Option<Instant>,
}

struct PageRow {
    path: String,
    views: Eased,
    frac: Eased,
    /// Places climbed since the last report — `None` for a chunk that wasn't
    /// on the board before.
    moved: Option<i64>,
}

/// Events per day for the period and for the period before it.
///
/// The points are kept in the chart's own coordinates so the datasets can borrow
/// them straight out of the dashboard rather than being rebuilt every frame.
#[derive(Default)]
struct EventTrend {
    current: Vec<(f64, f64)>,
    /// The earlier period, laid over the same x range so the two read as a
    /// comparison rather than as one series twice as long.
    previous: Vec<(f64, f64)>,
    /// Day-of-month labels, one per point in `current`.
    days: Vec<String>,
    /// The top event names, in rank order, in the chart's own coordinates.
    /// Drawn under the two period lines: every one of them is part of the
    /// total above it, so they cannot cross it and never need their own scale.
    names: Vec<(String, Vec<(f64, f64)>)>,
    total: f64,
    total_previous: f64,
    peak: f64,
}

/// Which panels are on screen. Hidden panels give their rows and columns back
/// to whatever is left, so hiding one is a layout change rather than a blank
/// rectangle.
struct Panels {
    vitals: bool,
    live: bool,
    chunks: bool,
    realms_ranked: bool,
    portals: bool,
    trend: bool,
    map: bool,
    events: bool,
}

impl Panels {
    fn any(&self) -> bool {
        self.vitals
            || self.live
            || self.chunks
            || self.realms_ranked
            || self.portals
            || self.trend
            || self.map
            || self.events
    }

    /// Panels that live in the left-hand column, top to bottom.
    ///
    /// Three of them, and the column is there for any one of them. It used to
    /// be there for the vitals alone, which meant `5` did not hide a panel so
    /// much as close half the dashboard — the chart and the map went with it,
    /// and the only way to get them back was to turn the figures on again.
    fn left_any(&self) -> bool {
        self.vitals || self.map || self.events
    }

    /// Panels that live in the right-hand column, top to bottom.
    fn right_any(&self) -> bool {
        self.live || self.chunks || self.realms_ranked || self.portals || self.trend
    }
}

/// A change in the realtime count, shown in the live feed until it ages out.
struct FeedEvent {
    delta: f64,
    at: Instant,
}

/// What Shift+D is asking about.
///
/// anacraft does not delete properties, so this is a question about *this*
/// config and nothing more: the property leaves the tab rotation and stays in
/// Google with all of its data. The overlay says so, because a confirmation
/// box in a dashboard is exactly where somebody would assume otherwise.
struct Forget {
    id: String,
    name: String,
}

/// What `c` copied, and for which property.
///
/// The strings are on the overlay as well as on the clipboard, because OSC 52
/// is a request a terminal is free to ignore and it never says which it did.
/// A line somebody can select with the mouse is the fallback that always
/// works.
struct Wired {
    property: String,
    /// Endpoint and token in one string — what goes on the clipboard, because
    /// a URL field is all most of these clients have.
    link: String,
    command: String,
    /// Set when there was no connector to work out — no home directory to
    /// keep the secret in, most likely.
    trouble: Option<String>,
}

impl Wired {
    /// Work out the connector for a property and ask the terminal to take it.
    fn copy(property: &Property) -> Wired {
        let mut wired = Wired {
            property: property.display(),
            link: String::new(),
            command: String::new(),
            trouble: None,
        };
        match crate::serve::connector(&property.id) {
            Ok(wire) => {
                to_clipboard(&wire.link);
                wired.link = wire.link;
                wired.command = wire.command;
            }
            Err(err) => wired.trouble = Some(err.to_string()),
        }
        wired
    }
}

/// Hand a line to the clipboard of the machine somebody is sitting at.
///
/// OSC 52 rather than a clipboard crate, because the terminal running this
/// may be on the far end of an ssh connection — in which case the X or
/// Wayland clipboard this process could reach is on the wrong computer. The
/// terminal never answers, and some are configured not to listen at all, so
/// this cannot report success; that is what the overlay is for.
fn to_clipboard(text: &str) {
    use base64::Engine;
    use std::io::Write;

    let payload = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{payload}\x07");
    let _ = out.flush();
}

struct Dash {
    title: String,
    days: u32,
    metrics: Vec<MetricRow>,
    daily: Vec<f64>,
    pages: Vec<PageRow>,
    /// Events per day, this period against the last.
    events: EventTrend,
    live: Eased,
    live_raw: f64,
    /// Realtime samples, oldest first — the live sparkline scrolls off this.
    history: VecDeque<f64>,
    /// Users by country over the period — where they came from, and the map's
    /// base layer.
    realms: Vec<(String, f64)>,
    /// Countries with somebody on the site right now, lit on top of the base.
    live_realms: Vec<(String, f64)>,
    /// Sessions by source and medium — who is sending the traffic.
    portals: Vec<(String, f64)>,
    feed: VecDeque<FeedEvent>,
    updated: String,
    /// Set when a refresh fails; the last good numbers stay on screen.
    error: Option<String>,
    /// Report parts still in the air. The spinner is on while this is non-zero.
    in_flight: u8,
    live_fetching: bool,
    panels: Panels,
    help: bool,
    /// The property Shift+D is asking about, if it is asking. Holding the name
    /// and id here rather than reading the rotation at draw time keeps the
    /// overlay showing what was confirmed, not what the rotation says after.
    forget: Option<Forget>,
    /// The connector `c` put on the clipboard, while its overlay is up. Held
    /// here for the same reason: it shows what was copied, not what the
    /// rotation has moved on to.
    wired: Option<Wired>,
    /// Highest realtime count seen this session — the meter's high-water mark.
    peak: f64,
    /// Drives every phase-based effect, so they all share one clock.
    started: Instant,
    last_report: Instant,
    report_every: Duration,
    live_every: Duration,
    /// Whether to wear the subscriber star in the header.
    supporter: bool,
    /// The plan this machine is on, when one is. Worn on the supporter line so
    /// a Pro or Elite knows the box is talking about the plan they bought,
    /// rather than a plan the CLI believes on their behalf.
    tier: Option<crate::license::Tier>,
    /// Running on synthetic data. Only the demo lets `s` flip `supporter`, so
    /// the Anacrafter treatment can be looked at before it is paid for.
    demo: bool,
    /// The signed-in Anacrafter's pixel face, worn in the top-right corner.
    avatar: Avatar,
    /// What the star says to a subscriber. Derived from the account the same
    /// way `avatar` is, and held rather than looked up per frame — the
    /// dashboard redraws sixty times a second and this must not read the
    /// token that often, nor change while somebody is looking at it.
    supporter_line: &'static str,
    /// The Anacrafter's number, worn beside the word itself. `None` where the
    /// service has none to give — an older one, or an account that has never
    /// paid — and the line simply reads as it always did.
    founder: Option<u32>,
    /// When each panel joined the board, by `Tile::index`. A tile takes its
    /// place the way a new window joins a tiling manager's stack — at the
    /// back, in the order it was switched on, not back in the slot its key
    /// would suggest. Hiding 3 from `1 3 8` and bringing it back leaves
    /// `1 8 3`.
    joined: [u32; 8],
    /// The next place at the back of the board.
    next_join: u32,
}

impl Dash {
    /// Flips a panel, and when it comes on puts it at the back of the board.
    ///
    /// Every toggle goes through here, so the order the tiles are drawn in is
    /// the order they were switched on. A tile that has just been brought
    /// back is the newest tile, not the one its key number says it is: from
    /// `1 3 8`, hiding 3 leaves `1 8`, and pressing 3 again gives `1 8 3`.
    fn toggle(&mut self, tile: Tile) {
        let switch = tile.switch(&mut self.panels);
        *switch = !*switch;
        if *switch {
            self.joined[tile.index()] = self.next_join;
            self.next_join += 1;
        }
    }

    fn new(
        title: String,
        days: u32,
        snapshot: Snapshot,
        live: f64,
        realms: Vec<(String, f64)>,
        report_every: Duration,
        live_every: Duration,
    ) -> Dash {
        let mut dash = Dash {
            // Nothing has been pressed yet, so the board opens in key order.
            joined: [0, 1, 2, 3, 4, 5, 6, 7],
            next_join: 8,
            title,
            days,
            metrics: Vec::new(),
            daily: Vec::new(),
            pages: Vec::new(),
            events: EventTrend::default(),
            live: Eased::new(live),
            live_raw: live,
            history: VecDeque::from(vec![live]),
            realms: Vec::new(),
            live_realms: realms,
            portals: Vec::new(),
            feed: VecDeque::new(),
            updated: stamp(),
            error: None,
            in_flight: 0,
            live_fetching: false,
            // The board opens on the first five keys and holds 6, 7 and 8 in
            // reserve. All eight used to be on, which meant the dashboard
            // opened at its most crowded and every key a newcomer pressed
            // took something away — the wrong direction for a first move.
            // Five is the board at a glance: the chart, the realtime panel,
            // the map, the pages and the figures, three across over two,
            // every tile at a size worth reading. What is left is a panel
            // you go and get, and the number that fetches it is the number
            // in its title.
            panels: Panels {
                events: true,
                live: true,
                map: true,
                chunks: true,
                vitals: true,
                realms_ranked: false,
                trend: false,
                portals: false,
            },
            help: false,
            forget: None,
            wired: None,
            peak: live.max(1.0),
            started: Instant::now(),
            last_report: Instant::now(),
            report_every,
            live_every,
            supporter: false,
            tier: None,
            demo: false,
            avatar: Avatar::demo(),
            supporter_line: crate::license::demo_supporter_line(),
            founder: Some(crate::license::DEMO_FOUNDER),
        };
        dash.apply_report(snapshot);
        // The constructor's own report shouldn't set every row flashing.
        for row in &mut dash.metrics {
            row.flash = None;
        }
        dash
    }

    /// Re-point the dashboard at another property. The numbers on screen
    /// belong to the property we are leaving, so anything that is a running
    /// tally of *this* site resets; the metric rows are left in place so the
    /// eased values animate across instead of blanking the layout.
    fn switch_to(&mut self, title: String, settings: Settings) {
        self.title = title;
        self.days = settings.days;
        self.report_every = Duration::from_secs(settings.refresh.max(5));
        self.live_every = Duration::from_secs(settings.live_refresh.max(LIVE_FLOOR));
        self.error = None;
        self.feed.clear();
        self.history.clear();
        self.history.push_back(self.live_raw);
        self.peak = self.live_raw.max(1.0);
        for row in &mut self.metrics {
            row.flash = None;
        }
    }

    fn apply_report(&mut self, snapshot: Snapshot) {
        self.apply_totals(snapshot.current, snapshot.previous);
        self.apply_trend(snapshot.daily);
        self.apply_pages(snapshot.pages);
        self.apply_events(snapshot.events);
        self.realms = snapshot.realms;
        self.portals = snapshot.portals;
    }

    fn apply_totals(&mut self, current: Vec<f64>, previous: Vec<f64>) {
        let now = Instant::now();

        for (i, _) in OVERVIEW.iter().enumerate() {
            let current = current.get(i).copied().unwrap_or(0.0);
            let previous = previous.get(i).copied().unwrap_or(0.0);
            let frac = crate::render::bar_fraction(OVERVIEW[i], current, previous);

            match self.metrics.get_mut(i) {
                Some(row) => {
                    if row.value.target != current {
                        row.flash = Some(now);
                    }
                    row.value.to(current);
                    row.frac.to(frac);
                    row.previous = previous;
                }
                None => self.metrics.push(MetricRow {
                    value: Eased::new(current),
                    frac: Eased::new(frac),
                    previous,
                    flash: Some(now),
                }),
            }
        }

        self.updated = stamp();
        self.error = None;
    }

    fn apply_trend(&mut self, daily: Vec<f64>) {
        self.daily = daily;
        self.updated = stamp();
        self.error = None;
    }

    fn apply_pages(&mut self, pages: Vec<(String, f64)>) {
        let peak = pages.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max);

        // The board as it stood, so a chunk that climbed can say so.
        let before: Vec<String> = self.pages.iter().map(|row| row.path.clone()).collect();
        let had_board = !before.is_empty();

        self.pages.truncate(pages.len());
        for (i, (path, views)) in pages.iter().enumerate() {
            let frac = if peak > 0.0 { views / peak } else { 0.0 };
            let moved = match before.iter().position(|seen| seen == path) {
                Some(was) => Some(was as i64 - i as i64),
                // Only call something new if there was a board to be new to.
                None if had_board => None,
                None => Some(0),
            };
            match self.pages.get_mut(i) {
                // Rows are positional: when a page changes rank, the label
                // swaps and the bar animates from whatever was there before,
                // which reads as the ranking rearranging itself.
                Some(row) => {
                    row.path = path.clone();
                    row.views.to(*views);
                    row.frac.to(frac);
                    row.moved = moved;
                }
                None => self.pages.push(PageRow {
                    path: path.clone(),
                    views: Eased::new(*views),
                    frac: Eased::new(frac),
                    moved,
                }),
            }
        }

        self.updated = stamp();
        self.error = None;
    }

    /// The two periods are plotted against one x range, so day 1 of this period
    /// sits under day 1 of the last however many days each actually returned.
    fn apply_events(&mut self, counts: EventCounts) {
        let EventCounts {
            current,
            previous,
            names,
        } = counts;
        let points = |counts: &[(String, f64)]| -> Vec<(f64, f64)> {
            counts
                .iter()
                .enumerate()
                .map(|(i, (_, count))| (i as f64, *count))
                .collect()
        };

        self.events = EventTrend {
            days: current.iter().map(|(date, _)| day_of_month(date)).collect(),
            total: current.iter().map(|(_, count)| count).sum(),
            total_previous: previous.iter().map(|(_, count)| count).sum(),
            // One scale for every line, or the comparison is meaningless. The
            // name series are parts of `current` and cannot exceed it, so the
            // two period lines still decide the ceiling.
            peak: current
                .iter()
                .chain(previous.iter())
                .map(|(_, count)| *count)
                .fold(0.0_f64, f64::max),
            names: names
                .into_iter()
                .map(|(name, series)| {
                    let points = series
                        .into_iter()
                        .enumerate()
                        .map(|(i, count)| (i as f64, count))
                        .collect();
                    (name, points)
                })
                .collect(),
            current: points(&current),
            previous: points(&previous),
        };

        self.updated = stamp();
        self.error = None;
    }

    fn apply_live(&mut self, live: f64, realms: Vec<(String, f64)>) {
        self.live_realms = realms;
        let delta = live - self.live_raw;
        if delta != 0.0 {
            self.feed.push_front(FeedEvent {
                delta,
                at: Instant::now(),
            });
        }
        self.live_raw = live;
        self.live.to(live);
        self.peak = self.peak.max(live);
    }

    /// Samples the *displayed* value onto the graph. Called on its own clock,
    /// not on the poll: that is what keeps the trace scrolling — and easing
    /// between polls — instead of standing still until the next sample lands.
    fn trace(&mut self) {
        self.history.push_back(self.live.shown);
        while self.history.len() > HISTORY {
            self.history.pop_front();
        }
    }

    fn step(&mut self, dt: f64) {
        for row in &mut self.metrics {
            row.value.step(dt);
            row.frac.step(dt);
        }
        for row in &mut self.pages {
            row.views.step(dt);
            row.frac.step(dt);
        }
        self.live.step(dt);
        while self.feed.back().is_some_and(|e| e.at.elapsed() > FEED_TTL) {
            self.feed.pop_back();
        }
    }

    /// Seconds-based clock every phase effect reads from.
    fn phase(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
}

fn stamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

// ------------------------------------------------------------------ fetch ---

/// The headline totals and the period they are measured against.
async fn fetch_totals(client: &Ga, property: &str, days: u32) -> Result<(Vec<f64>, Vec<f64>)> {
    let metrics: Vec<&str> = OVERVIEW.iter().map(|m| m.api).collect();

    let (current, previous) = tokio::try_join!(
        client.report(
            property,
            ReportRequest::new(&metrics).range(DateRange::last_days(days)),
        ),
        client.report(
            property,
            ReportRequest::new(&metrics).range(DateRange::previous_days(days)),
        )
    )?;

    Ok((
        (0..OVERVIEW.len()).map(|i| current.total(i)).collect(),
        (0..OVERVIEW.len()).map(|i| previous.total(i)).collect(),
    ))
}

async fn fetch_trend(client: &Ga, property: &str, days: u32) -> Result<Vec<f64>> {
    let trend = client
        .report(
            property,
            ReportRequest::new(&["totalUsers"])
                .by(&["date"])
                .range(DateRange::last_days(days)),
        )
        .await?;

    // GA returns date rows unordered; the sparkline needs them chronological.
    let mut rows = trend.rows.clone();
    rows.sort_by(|a, b| a.dimension(0).cmp(b.dimension(0)));
    Ok(rows.iter().map(|r| r.metric(0)).collect())
}

/// Users by country over the period — the map's base layer.
async fn fetch_realms(client: &Ga, property: &str, days: u32) -> Result<Vec<(String, f64)>> {
    let report = client
        .report(
            property,
            ReportRequest::new(&["totalUsers"])
                .by(&["country"])
                .range(DateRange::last_days(days))
                .top("totalUsers", 40),
        )
        .await?;

    Ok(report
        .rows
        .iter()
        .map(|r| (r.dimension(0).to_string(), r.metric(0)))
        .collect())
}

async fn fetch_pages(client: &Ga, property: &str, days: u32) -> Result<Vec<(String, f64)>> {
    let pages = client
        .report(
            property,
            ReportRequest::new(&["screenPageViews"])
                .by(&["pagePath"])
                .range(DateRange::last_days(days))
                .top("screenPageViews", 8),
        )
        .await?;

    Ok(pages
        .rows
        .iter()
        .map(|r| (r.dimension(0).to_string(), r.metric(0)))
        .collect())
}

/// Who sent the sessions, ranked.
///
/// `sessionSourceMedium` rather than `sessionSource`: the same site can be both
/// a link somebody wrote and an ad somebody bought, and a panel that answers
/// "who is mentioning us" has to keep those apart. Eight rows, the depth the
/// panel can draw.
async fn fetch_portals(client: &Ga, property: &str, days: u32) -> Result<Vec<(String, f64)>> {
    let portals = client
        .report(
            property,
            ReportRequest::new(&["sessions"])
                .by(&["sessionSourceMedium"])
                .range(DateRange::last_days(days))
                .top("sessions", 8),
        )
        .await?;

    Ok(portals
        .rows
        .iter()
        .map(|r| (r.dimension(0).to_string(), r.metric(0)))
        .collect())
}

/// Everything the events chart draws: this period by day, the period before it,
/// and the top event names broken out over the same days.
///
/// Three requests rather than one. GA has no period comparison in a single
/// report — asking for both date ranges at once returns them interleaved with
/// no way to tell which range a row came from — and the breakdown needs a
/// second dimension, which would otherwise split every day's total into rows
/// the headline cannot be summed back out of.
///
/// Two of the three actually leave the machine. The middle one is the period
/// before this one, which `settled` answers from memory for as long as it
/// cannot have changed, so the name breakdown is drawn for what the old
/// two-line chart already cost.
async fn fetch_events(client: &Ga, property: &str, days: u32) -> Result<EventCounts> {
    let (current, previous, names) = tokio::try_join!(
        events_by_day(client, property, DateRange::last_days(days)),
        settled_previous(client, property, days),
        events_by_name(client, property, days),
    )?;

    // The names are laid on the days the totals actually came back with, not
    // on the days that were asked for: a property with a quiet Sunday has no
    // Sunday row anywhere, and the axis is built from `current`.
    let days: Vec<&str> = current.iter().map(|(date, _)| date.as_str()).collect();
    Ok(EventCounts {
        names: align(&days, names),
        current,
        previous,
    })
}

/// One report's worth of event counts by day, chronological.
///
/// GA returns date rows unordered, and a line chart needs them in order or it
/// draws the month as a scribble.
async fn events_by_day(client: &Ga, property: &str, range: DateRange) -> Result<Daily> {
    let report = client
        .report(
            property,
            ReportRequest::new(&["eventCount"])
                .by(&["date"])
                .range(range),
        )
        .await?;

    let mut rows: Daily = report
        .rows
        .iter()
        .map(|row| (row.dimension(0).to_string(), row.metric(0)))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(rows)
}

/// How many `date` × `eventName` cells to ask for.
///
/// One request either way, so this is only a question of where the tail is cut.
/// Two thousand covers a quarter's window on a site with twenty distinct
/// events, and the cells come back ranked by count — so the days that make a
/// name one of the top few are the first rows GA hands over, and a cut tail
/// costs a small day on a small event rather than a whole line.
const NAME_CELLS: i32 = 2_000;

/// The top event names over the period, each as its own daily series.
async fn events_by_name(client: &Ga, property: &str, days: u32) -> Result<Vec<(String, Daily)>> {
    let report = client
        .report(
            property,
            ReportRequest::new(&["eventCount"])
                .by(&["date", "eventName"])
                .range(DateRange::last_days(days))
                .top("eventCount", NAME_CELLS),
        )
        .await?;

    // GA ranked the cells, not the names: `page_view` on its best day outranks
    // `session_start` on its best day, and neither ordering says which name is
    // bigger over the period. So the totals are summed here and the ranking is
    // done on them.
    let mut totals: HashMap<&str, f64> = HashMap::new();
    let mut series: HashMap<&str, Daily> = HashMap::new();
    for row in &report.rows {
        let (date, name, count) = (row.dimension(0), row.dimension(1), row.metric(0));
        *totals.entry(name).or_default() += count;
        series
            .entry(name)
            .or_default()
            .push((date.to_string(), count));
    }

    let mut ranked: Vec<(String, Daily)> = series
        .into_iter()
        .map(|(name, rows)| (name.to_string(), rows))
        .collect();
    // Ties broken by name so the colours a site sees are the same on every
    // refresh — a legend that reshuffles itself every thirty seconds is worse
    // than no legend.
    ranked.sort_by(|a, b| {
        totals[b.0.as_str()]
            .total_cmp(&totals[a.0.as_str()])
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked.truncate(EVENT_NAME_LINES);
    for (_, rows) in ranked.iter_mut() {
        rows.sort_by(|a, b| a.0.cmp(&b.0));
    }
    Ok(ranked)
}

/// Lay each name's counts on the period's days.
///
/// A name with nothing on Tuesday has no Tuesday row, and a series that simply
/// skipped it would draw Wednesday's count above Tuesday's tick — every point
/// after a quiet day sliding one day left of the day it belongs to, which on a
/// chart of five lines is five wrong stories rather than one.
fn align(days: &[&str], names: Vec<(String, Daily)>) -> Vec<(String, Vec<f64>)> {
    names
        .into_iter()
        .map(|(name, rows)| {
            let found: HashMap<&str, f64> = rows
                .iter()
                .map(|(date, count)| (date.as_str(), *count))
                .collect();
            let series = days
                .iter()
                .map(|day| found.get(day).copied().unwrap_or(0.0))
                .collect();
            (name, series)
        })
        .collect()
}

/// How long the previous period's counts are trusted without asking again.
///
/// They are the one thing on this chart that cannot change: the window ends the
/// day before the current period begins, so it is finished data about finished
/// days. Asking for it again every thirty seconds is a request per refresh, for
/// as long as the dashboard is open, whose answer is already on the screen.
///
/// What does move is which days the window covers, and that happens once, at
/// midnight — in the property's reporting timezone, which is not necessarily
/// this machine's. So this is a time-to-live rather than a date: half an hour
/// is long enough to take the request out of the refresh entirely, and short
/// enough that the roll-over is never visible for long whatever timezone the
/// property keeps.
const SETTLED_FOR: Duration = Duration::from_secs(30 * 60);

/// The previous period, as last fetched.
///
/// One slot rather than a map: a dashboard reads one property over one window
/// at a time, and the entry a second property would evict is one this one is
/// no longer drawing.
struct Settled {
    property: String,
    days: u32,
    at: Instant,
    counts: Daily,
}

static SETTLED: Mutex<Option<Settled>> = Mutex::new(None);

/// The period before this one, from memory when that is still honest.
async fn settled_previous(client: &Ga, property: &str, days: u32) -> Result<Daily> {
    if let Some(counts) = settled_hit(property, days, Instant::now()) {
        return Ok(counts);
    }

    let counts = events_by_day(client, property, DateRange::previous_days(days)).await?;
    // A poisoned lock means some other task panicked holding it. That is worth
    // nothing here: the cache is an optimisation, and losing it costs one
    // request per refresh rather than a dashboard.
    if let Ok(mut slot) = SETTLED.lock() {
        *slot = Some(Settled {
            property: property.to_string(),
            days,
            at: Instant::now(),
            counts: counts.clone(),
        });
    }
    Ok(counts)
}

/// What the slot has to say about this property and window, if anything.
///
/// Keyed on both: switching property or changing the window with `[`/`]` asks a
/// different question, and answering it with the last one's numbers would draw
/// somebody else's fortnight under this one.
fn settled_hit(property: &str, days: u32, now: Instant) -> Option<Daily> {
    let slot = SETTLED.lock().ok()?;
    let held = slot.as_ref()?;
    let fresh = now.saturating_duration_since(held.at) < SETTLED_FOR;
    (held.property == property && held.days == days && fresh).then(|| held.counts.clone())
}

/// `YYYYMMDD` down to the day, for the chart's x axis.
fn day_of_month(date: &str) -> String {
    date.get(6..8)
        .map(|day| day.trim_start_matches('0').to_string())
        .filter(|day| !day.is_empty())
        .unwrap_or_else(|| date.to_string())
}

/// The whole set at once, for the fetch that happens before the screen is
/// taken over. The parts run concurrently, so this costs one round trip rather
/// than one per part.
async fn fetch_report(client: &Ga, property: &str, days: u32) -> Result<Snapshot> {
    let (totals, daily, pages, realms, portals, events) = tokio::try_join!(
        fetch_totals(client, property, days),
        fetch_trend(client, property, days),
        fetch_pages(client, property, days),
        fetch_realms(client, property, days),
        fetch_portals(client, property, days),
        fetch_events(client, property, days)
    )?;

    Ok(Snapshot {
        current: totals.0,
        previous: totals.1,
        daily,
        pages,
        realms,
        portals,
        events,
    })
}

/// The realtime count and its breakdown by country, which is what the map
/// lights up from. Rows are summed rather than read off `totals`: a dimensioned
/// realtime request doesn't come back with aggregates.
async fn fetch_live(client: &Ga, property: &str) -> Result<(f64, Vec<(String, f64)>)> {
    let report = client
        .realtime(
            property,
            ReportRequest::new(&["activeUsers"])
                .by(&["country"])
                .top("activeUsers", 30),
        )
        .await?;

    let realms: Vec<(String, f64)> = report
        .rows
        .iter()
        .map(|r| (r.dimension(0).to_string(), r.metric(0)))
        .collect();
    let total = realms.iter().map(|(_, users)| users).sum();
    Ok((total, realms))
}

/// Where the dashboard's numbers come from. The event loop is written against
/// this rather than against `Ga`, so `dash --demo` exercises exactly the same
/// animation path as a connected property.
enum Source {
    Api { client: Arc<Ga>, property: String },
    Demo(std::sync::Mutex<Synthetic>),
}

/// The requests a report pass is made of.
#[derive(Clone, Copy)]
enum Part {
    Totals,
    Trend,
    Pages,
    Realms,
    Portals,
    Events,
}

impl Source {
    /// Kicks off a report pass and returns how many parts to expect back. The
    /// parts run concurrently and each is sent the moment it lands, so the
    /// dashboard fills in as data comes in instead of in one jump at the end.
    fn request_report(&self, days: u32, tx: &UnboundedSender<Update>) -> u8 {
        match self {
            Source::Api { client, property } => {
                let spawn = |part: Part| {
                    let (client, property, tx) = (client.clone(), property.clone(), tx.clone());
                    tokio::spawn(async move {
                        let update = match part {
                            Part::Totals => fetch_totals(&client, &property, days)
                                .await
                                .map(|(current, previous)| Update::Totals { current, previous }),
                            Part::Trend => fetch_trend(&client, &property, days)
                                .await
                                .map(Update::Trend),
                            Part::Pages => fetch_pages(&client, &property, days)
                                .await
                                .map(Update::Pages),
                            Part::Realms => fetch_realms(&client, &property, days)
                                .await
                                .map(Update::Realms),
                            Part::Portals => fetch_portals(&client, &property, days)
                                .await
                                .map(Update::Portals),
                            Part::Events => fetch_events(&client, &property, days)
                                .await
                                .map(Update::Events),
                        };
                        // Keep showing stale numbers rather than tearing the
                        // screen down.
                        let _ =
                            tx.send(update.unwrap_or_else(|err| Update::Failed(err.to_string())));
                    });
                };
                spawn(Part::Totals);
                spawn(Part::Trend);
                spawn(Part::Pages);
                spawn(Part::Realms);
                spawn(Part::Portals);
                spawn(Part::Events);
                6
            }
            Source::Demo(synthetic) => {
                let snapshot = synthetic.lock().unwrap().report(&mut rand::thread_rng());
                let _ = tx.send(Update::Totals {
                    current: snapshot.current,
                    previous: snapshot.previous,
                });
                let _ = tx.send(Update::Trend(snapshot.daily));
                let _ = tx.send(Update::Pages(snapshot.pages));
                let _ = tx.send(Update::Realms(snapshot.realms));
                let _ = tx.send(Update::Portals(snapshot.portals));
                let _ = tx.send(Update::Events(snapshot.events));
                6
            }
        }
    }

    /// Point an API source at a different property. Inert for the demo, which
    /// has only its synthetic site.
    fn set_property(&mut self, id: &str) {
        if let Source::Api { property, .. } = self {
            *property = id.to_string();
        }
    }

    fn request_live(&self, tx: &UnboundedSender<Update>) {
        match self {
            Source::Api { client, property } => {
                let (client, property, tx) = (client.clone(), property.clone(), tx.clone());
                tokio::spawn(async move {
                    // Realtime is the flakiest endpoint of the set; a failure
                    // there holds the last count rather than papering the
                    // dashboard with an error.
                    if let Ok((total, realms)) = fetch_live(&client, &property).await {
                        let _ = tx.send(Update::Live { total, realms });
                    }
                });
            }
            Source::Demo(synthetic) => {
                let (total, realms) = synthetic.lock().unwrap().live(&mut rand::thread_rng());
                let _ = tx.send(Update::Live { total, realms });
            }
        }
    }
}

// ------------------------------------------------------------------- loop ---

/// Cadence and window the dashboard runs at, after flags and the property's
/// own saved settings have been folded together.
#[derive(Clone, Copy)]
pub struct Settings {
    pub days: u32,
    pub refresh: u64,
    pub live_refresh: u64,
}

impl Settings {
    /// This property's overrides on top of the resolved defaults. Switching to
    /// a property that saved nothing lands back on the defaults rather than
    /// inheriting whatever the previous property used.
    fn for_property(&self, property: &Property) -> Settings {
        Settings {
            days: property.days.unwrap_or(self.days),
            refresh: property.refresh.unwrap_or(self.refresh),
            live_refresh: property.live_refresh.unwrap_or(self.live_refresh),
        }
    }
}

pub async fn run(cfg: &Config, property: &str, settings: Settings) -> Result<()> {
    let client = Arc::new(Ga::new()?);

    // Tab cycles this list. Start it on the property we were asked for, so
    // `--property` decides where the dashboard opens, not just what it can
    // reach. A property that isn't in the config still runs, alone.
    let mut rotation: Vec<Property> = cfg.properties.clone();
    if !rotation.iter().any(|p| p.id == property) {
        rotation.insert(
            0,
            Property {
                id: property.to_string(),
                ..Property::default()
            },
        );
    }
    let index = rotation
        .iter()
        .position(|p| p.id == property)
        .unwrap_or_default();
    let opening = settings.for_property(&rotation[index]);
    let title = rotation[index].display();

    // Fetch before taking over the screen so auth/API errors print normally.
    let snapshot = fetch_report(&client, property, opening.days).await?;
    let (live, realms) = fetch_live(&client, property)
        .await
        .unwrap_or((0.0, Vec::new()));

    let source = Source::Api {
        client,
        property: property.to_string(),
    };
    drive(
        source,
        title,
        snapshot,
        live,
        realms,
        settings,
        rotation,
        index,
        cfg.supporter,
        cfg.tier(),
    )
    .await
}

/// The dashboard on synthetic data — no account, but the same code path, which
/// is what makes it usable for screenshots and for tuning the animation.
pub async fn run_demo(days: u32, refresh: u64, live_refresh: u64) -> Result<()> {
    let mut synthetic = Synthetic::new();
    let mut rng = rand::thread_rng();
    let snapshot = synthetic.report(&mut rng);
    let (live, realms) = synthetic.live(&mut rng);

    let source = Source::Demo(std::sync::Mutex::new(synthetic));
    drive(
        source,
        "Contoso Labs (demo)".to_string(),
        snapshot,
        live,
        realms,
        Settings {
            days,
            refresh,
            live_refresh,
        },
        Vec::new(),
        0,
        false,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    source: Source,
    title: String,
    snapshot: Snapshot,
    live: f64,
    realms: Vec<(String, f64)>,
    settings: Settings,
    rotation: Vec<Property>,
    index: usize,
    supporter: bool,
    tier: Option<crate::license::Tier>,
) -> Result<()> {
    let opening = match rotation.get(index) {
        Some(property) => settings.for_property(property),
        None => settings,
    };
    let mut source = source;
    let mut dash = Dash::new(
        title,
        opening.days,
        snapshot,
        live,
        realms,
        Duration::from_secs(opening.refresh.max(5)),
        Duration::from_secs(opening.live_refresh.max(LIVE_FLOOR)),
    );
    dash.supporter = supporter;
    dash.tier = tier;
    dash.demo = matches!(source, Source::Demo(_));
    // The demo keeps the fixed face even on a machine that is signed in: it is
    // what the site's captures show, and it is not this account's to hand out.
    if !dash.demo {
        dash.avatar = Avatar::for_account();
        dash.supporter_line = crate::license::supporter_line();
        // Read once, here, rather than per frame: it is a number that is
        // assigned once and never moves while somebody is looking at it.
        dash.founder = crate::license::Record::load().status.founder;
    }

    let mut terminal = ratatui::init();
    // Ctrl+digit only reaches an application in terminals that speak the Kitty
    // keyboard protocol; without this they send the bare digit (or nothing).
    // The plain digits keep working either way, so this is an upgrade rather
    // than a requirement.
    let enhanced = matches!(supports_keyboard_enhancement(), Ok(true));
    if enhanced {
        let _ = execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let mut rotation = rotation;
    let result = event_loop(
        &mut terminal,
        &mut source,
        &mut dash,
        &mut rotation,
        index,
        settings,
    )
    .await;
    if enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();

    // Persist what the session settled on, so the next launch starts there.
    //
    // Both halves of this are about the property the dashboard *ended* on
    // rather than the one it opened on. Tab used to move the view and nothing
    // else, so a person who tabbed to another property and quit found every
    // later command — `craft overview`, a cron'd `craft watch` — still reading
    // the one they had left, while the config comment told them Tab was how
    // properties are switched. The theme had the same fault from the other
    // side: pressing `t` after tabbing wrote the palette onto the property no
    // longer on screen.
    //
    // Landing is what commits it, not passing through. Tabbing along the
    // rotation to look at each property in turn costs nothing until you quit
    // somewhere, which is the reading of "current" that does not repoint a
    // scheduled job under somebody halfway through browsing.
    let settled = result
        .as_ref()
        .ok()
        .and_then(|id| id.clone())
        .filter(|id| !id.is_empty());

    if let Ok(mut cfg) = crate::config::Config::load() {
        settle(&mut cfg, settled, theme::palette().name.to_string());
        let _ = cfg.save();
    }
    result.map(|_| ())
}

/// Write the property and palette a dashboard session ended on into the config.
///
/// Split out from `drive` because the terminal around it cannot be driven from
/// a test, and this is the part with a decision in it.
fn settle(cfg: &mut crate::config::Config, landed: Option<String>, theme: String) {
    // A property that is not in the config cannot be made active: it was
    // reached with `--property`, and one run is not a decision to switch.
    let target = landed
        .filter(|id| cfg.find(id).is_some())
        .or_else(|| cfg.active.clone().filter(|id| cfg.find(id).is_some()));
    match target {
        // `upsert` sets `active` as well as returning the entry, so this is
        // the one write that moves both.
        Some(id) => cfg.upsert(&id, None).theme = Some(theme),
        None => cfg.theme = Some(theme),
    }
}

/// Point the dashboard at another property and refetch at once.
///
/// Shared by Tab and by the forget confirmation, which both leave the screen
/// showing numbers that belong to the property just left.
fn show(
    source: &mut Source,
    dash: &mut Dash,
    settings: &Settings,
    next: &Property,
    tx: &mpsc::UnboundedSender<Update>,
    last_live: &mut Instant,
) {
    let resolved = settings.for_property(next);
    source.set_property(&next.id);
    dash.switch_to(next.display(), resolved);
    // A property carrying its own palette should show it immediately, not on
    // the next launch.
    if let Some(name) = next.theme.as_deref() {
        theme::select(name);
    }
    dash.in_flight = source.request_report(dash.days, tx);
    dash.last_report = Instant::now();
    source.request_live(tx);
    dash.live_fetching = true;
    *last_live = Instant::now();
}

/// The property the dashboard was showing when it closed.
///
/// `None` for the demo, which walks an empty rotation — there is no property
/// there to make current, and a synthetic one must never be written to a real
/// config.
fn landed(rotation: &[Property], index: usize) -> Option<String> {
    rotation.get(index).map(|p| p.id.clone())
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    source: &mut Source,
    dash: &mut Dash,
    rotation: &mut Vec<Property>,
    mut index: usize,
    settings: Settings,
) -> Result<Option<String>> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut last_live = Instant::now();
    let mut last_frame = Instant::now();
    let mut last_trace = Instant::now();

    loop {
        // Everything waiting is applied before the frame is drawn, so a part
        // that lands mid-frame shows up on the very next one.
        while let Ok(update) = rx.try_recv() {
            match update {
                Update::Totals { current, previous } => {
                    dash.apply_totals(current, previous);
                    dash.in_flight = dash.in_flight.saturating_sub(1);
                }
                Update::Trend(daily) => {
                    dash.apply_trend(daily);
                    dash.in_flight = dash.in_flight.saturating_sub(1);
                }
                Update::Pages(pages) => {
                    dash.apply_pages(pages);
                    dash.in_flight = dash.in_flight.saturating_sub(1);
                }
                Update::Events(events) => {
                    dash.apply_events(events);
                    dash.in_flight = dash.in_flight.saturating_sub(1);
                }
                Update::Realms(realms) => {
                    dash.realms = realms;
                    dash.updated = stamp();
                    dash.in_flight = dash.in_flight.saturating_sub(1);
                }
                Update::Portals(portals) => {
                    dash.portals = portals;
                    dash.updated = stamp();
                    dash.in_flight = dash.in_flight.saturating_sub(1);
                }
                Update::Live { total, realms } => {
                    dash.apply_live(total, realms);
                    dash.live_fetching = false;
                }
                Update::Failed(err) => {
                    dash.error = Some(err);
                    dash.in_flight = dash.in_flight.saturating_sub(1);
                }
            }
        }

        let now = Instant::now();
        let dt = now.duration_since(last_frame).as_secs_f64();
        last_frame = now;
        dash.step(dt);
        if last_trace.elapsed() >= TRACE_EVERY {
            dash.trace();
            last_trace = now;
        }

        terminal.draw(|frame| draw(frame, dash))?;

        if dash.in_flight == 0 && dash.last_report.elapsed() >= dash.report_every {
            dash.in_flight = source.request_report(dash.days, &tx);
            dash.last_report = now;
        }
        if !dash.live_fetching && last_live.elapsed() >= dash.live_every {
            source.request_live(&tx);
            dash.live_fetching = true;
            last_live = now;
        }

        // The poll timeout is the frame budget: input wakes us early, and
        // otherwise this is the tick that advances the animation.
        if event::poll(FRAME)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Ok(landed(rotation, index))
                        }
                        // The confirmation owns the keyboard while it is up:
                        // `y` is the only key that does anything, and every
                        // other one cancels rather than falling through to a
                        // panel toggle nobody meant to press.
                        KeyCode::Char('y') | KeyCode::Char('Y') if dash.forget.is_some() => {
                            let target = dash.forget.take().expect("checked");
                            if let Ok(mut cfg) = crate::config::Config::load() {
                                if cfg.remove(&target.id) {
                                    let _ = cfg.save();
                                }
                            }
                            rotation.retain(|p| p.id != target.id);

                            // Nothing left to show. Quitting is the honest
                            // outcome — an empty dashboard would sit there
                            // redrawing the numbers of a property that is no
                            // longer configured.
                            if rotation.is_empty() {
                                return Ok(None);
                            }
                            index %= rotation.len();
                            show(
                                source,
                                dash,
                                &settings,
                                &rotation[index],
                                &tx,
                                &mut last_live,
                            );
                        }
                        _ if dash.forget.is_some() => dash.forget = None,
                        // Same rule for the connector: it is up to be read
                        // and copied from, so the next key puts it away
                        // rather than toggling a panel behind it.
                        _ if dash.wired.is_some() => dash.wired = None,
                        KeyCode::Char('q') => return Ok(landed(rotation, index)),
                        // Esc closes the help overlay first, so it isn't a
                        // surprise exit for anyone who opened it to look.
                        KeyCode::Esc if dash.help => dash.help = false,
                        KeyCode::Esc => return Ok(landed(rotation, index)),
                        KeyCode::Char('r') => {
                            dash.last_report = now - dash.report_every;
                            last_live = now - dash.live_every;
                        }
                        KeyCode::Char('?') | KeyCode::Char('h') => dash.help = !dash.help,
                        // The property in front of somebody is the one an
                        // assistant should be reading, so `c` copies that
                        // one's connector rather than the config's default.
                        KeyCode::Char('c') => {
                            if let Some(property) = rotation.get(index) {
                                dash.wired = Some(Wired::copy(property));
                            }
                        }
                        // Ctrl+digit and the bare digit do the same thing: the
                        // titles advertise Ctrl, but not every terminal can
                        // send it.
                        KeyCode::Char('1') | KeyCode::Char('e') => dash.toggle(Tile::Events),
                        KeyCode::Char('2') | KeyCode::Char('l') => dash.toggle(Tile::Live),
                        KeyCode::Char('3') | KeyCode::Char('m') => dash.toggle(Tile::Map),
                        KeyCode::Char('4') | KeyCode::Char('p') => dash.toggle(Tile::Chunks),
                        KeyCode::Char('5') | KeyCode::Char('v') => dash.toggle(Tile::Vitals),
                        KeyCode::Char('6') | KeyCode::Char('g') => dash.toggle(Tile::RealmsRanked),
                        KeyCode::Char('7') | KeyCode::Char('d') => dash.toggle(Tile::Trend),
                        // `o`, not `p`: the chunk list took that one, and a
                        // portal is a thing you come thrOugh.
                        KeyCode::Char('8') | KeyCode::Char('o') => dash.toggle(Tile::Portals),
                        // Shift, not a bare `d`: that one toggles the daily
                        // users panel and always has. A key people press to
                        // look at a chart is the wrong place to put anything
                        // that changes their config.
                        KeyCode::Char('D') if !dash.demo => {
                            if let Some(property) = rotation.get(index) {
                                dash.forget = Some(Forget {
                                    id: property.id.clone(),
                                    name: property.display(),
                                });
                            }
                        }
                        // The demo is where someone decides whether $2.99 is
                        // worth it, so it can wear the Anacrafter treatment on
                        // request. Gated to the demo: on real data the flag
                        // answers to the subscription lookup, and a key that
                        // granted it would make the box meaningless. The preview
                        // wears Pro so the higher plans get looked at too.
                        KeyCode::Char('s') if dash.demo => {
                            dash.supporter = !dash.supporter;
                            dash.tier = if dash.supporter {
                                Some(crate::license::Tier::Pro)
                            } else {
                                None
                            };
                        }
                        // Nothing to announce: every color on screen changes,
                        // which is the feedback.
                        KeyCode::Char('t') => {
                            theme::cycle();
                        }
                        // Like the theme key, but for the vocabulary.
                        KeyCode::Char('b') => {
                            theme::toggle_boring();
                        }
                        // Tab walks the configured properties. A one-property
                        // rotation has nothing to walk to, so the key is inert
                        // rather than redrawing the same numbers.
                        KeyCode::Tab | KeyCode::BackTab if rotation.len() > 1 => {
                            let step = if key.code == KeyCode::Tab {
                                1
                            } else {
                                rotation.len() - 1
                            };
                            index = (index + step) % rotation.len();
                            show(
                                source,
                                dash,
                                &settings,
                                &rotation[index],
                                &tx,
                                &mut last_live,
                            );
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

// -------------------------------------------------------------- synthetic ---

/// A small site having a good week, drifting on every poll so the demo shows
/// motion rather than a frozen frame.
struct Synthetic {
    current: Vec<f64>,
    previous: Vec<f64>,
    daily: Vec<f64>,
    live: f64,
}

impl Synthetic {
    fn new() -> Synthetic {
        Synthetic {
            current: vec![12_481.0, 18_203.0, 41_776.0, 312.0, 0.412, 214.0],
            previous: vec![11_450.0, 17_004.0, 39_210.0, 258.0, 0.478, 191.0],
            daily: vec![1402.0, 1288.0, 1531.0, 1495.0, 1760.0, 1834.0, 1971.0],
            live: 128.0,
        }
    }

    fn report(&mut self, rng: &mut impl Rng) -> Snapshot {
        // Counts creep up, the bounce rate wobbles, the day's last bar grows.
        for (i, value) in self.current.iter_mut().enumerate() {
            *value *= 1.0 + rng.gen_range(-0.004..0.012) * if i == 4 { 0.4 } else { 1.0 };
        }
        if let Some(today) = self.daily.last_mut() {
            *today *= 1.0 + rng.gen_range(-0.02..0.05);
        }

        let mut snapshot = Snapshot {
            current: self.current.clone(),
            previous: self.previous.clone(),
            daily: self.daily.clone(),
            pages: [
                "/",
                "/pricing",
                "/docs/quickstart",
                "/blog/mining-metrics",
                "/changelog",
                "/docs/api",
                "/about",
                "/login",
            ]
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let base = 9_400.0 / (i as f64 + 1.4);
                // Wide enough that neighbouring chunks trade places now and
                // then, which is the only way to see the movement markers.
                (path.to_string(), base * rng.gen_range(0.80..1.20))
            })
            .collect(),
            realms: [
                ("United States", 0.34),
                ("India", 0.14),
                ("Germany", 0.09),
                ("United Kingdom", 0.08),
                ("Brazil", 0.07),
                ("Japan", 0.06),
                ("Canada", 0.05),
                ("Australia", 0.04),
                ("Nigeria", 0.04),
                ("France", 0.03),
                ("Sweden", 0.02),
                ("Singapore", 0.02),
                ("South Africa", 0.02),
                ("Mexico", 0.02),
            ]
            .iter()
            .map(|(name, share)| {
                (
                    name.to_string(),
                    (self.current[0] * share * rng.gen_range(0.9..1.1)).round(),
                )
            })
            .collect(),
            portals: [
                ("google / organic", 0.31),
                ("(direct) / (none)", 0.24),
                ("news.ycombinator.com / referral", 0.14),
                ("github.com / referral", 0.10),
                ("reddit.com / referral", 0.07),
                ("bing / organic", 0.05),
                ("t.co / referral", 0.04),
                ("lobste.rs / referral", 0.03),
            ]
            .iter()
            .map(|(name, share)| {
                // Off sessions, not users: a portal is counted by the visits it
                // sent, which is what `craft portals` ranks by.
                (
                    name.to_string(),
                    (self.current[1] * share * rng.gen_range(0.9..1.1)).round(),
                )
            })
            .collect(),
            events: {
                // A fixed anchor date, not today's: the site's captures embed
                // these day labels, and a moving window would rewrite them on
                // every regeneration.
                let anchor = chrono::NaiveDate::from_ymd_opt(2026, 8, 19).unwrap();
                // A week that sags at the weekend and climbs into Monday. Shaped
                // rather than random, so the two periods cross somewhere and the
                // comparison has something to show.
                const NOW: [f64; 7] = [0.61, 0.72, 0.54, 0.33, 0.44, 0.87, 1.00];
                const BEFORE: [f64; 7] = [0.57, 0.48, 0.60, 0.38, 0.30, 0.66, 0.71];
                // Events outnumber page views: every view is one, plus the rest.
                let base = self.current[2] * 1.7 / 7.0;
                let series = |shape: &[f64; 7], offset: i64| -> Vec<(String, f64)> {
                    shape
                        .iter()
                        .enumerate()
                        .map(|(day, scale)| {
                            let back = offset + 6 - day as i64;
                            let date = anchor - chrono::Duration::days(back);
                            (date.format("%Y%m%d").to_string(), (base * scale).round())
                        })
                        .collect()
                };
                // The names under the total, as shares of the day.
                //
                // Not a partition, and a real property's are not either: GA
                // counts a page view as `page_view` and again inside
                // `user_engagement`, so these overlap on purpose. What the
                // demo has to show is four ranked lines that sit under the
                // headline and keep their order, which is exactly what the
                // chart claims about a real site.
                const NAMES: [(&str, f64); 4] = [
                    ("page_view", 0.58),
                    ("user_engagement", 0.31),
                    ("scroll", 0.17),
                    ("session_start", 0.09),
                ];
                let current = series(&NOW, 0);
                EventCounts {
                    names: NAMES
                        .iter()
                        .map(|(name, share)| {
                            let daily = current
                                .iter()
                                .map(|(_, count)| (count * share).round())
                                .collect();
                            (name.to_string(), daily)
                        })
                        .collect(),
                    previous: series(&BEFORE, 7),
                    current,
                }
            },
        };

        // GA returns these ranked; the jitter above would otherwise leave them
        // in their original order with the values out of sequence.
        snapshot.pages.sort_by(|a, b| b.1.total_cmp(&a.1));
        snapshot
    }

    fn live(&mut self, rng: &mut impl Rng) -> (f64, Vec<(String, f64)>) {
        // Random walk with a pull back toward 128, so it wanders without
        // drifting off the panel.
        let pull = (128.0 - self.live) * 0.15;
        self.live = (self.live + pull + rng.gen_range(-9.0..9.0))
            .max(0.0)
            .round();

        // Split the walkers over a plausible spread of countries, so the map
        // has something to light up.
        let realms = [
            ("United States", 0.34),
            ("India", 0.14),
            ("Germany", 0.09),
            ("United Kingdom", 0.08),
            ("Brazil", 0.07),
            ("Japan", 0.06),
            ("Canada", 0.05),
            ("Australia", 0.04),
            ("Nigeria", 0.04),
            ("France", 0.03),
        ]
        .iter()
        .map(|(name, share)| {
            let jitter = rng.gen_range(0.85..1.15);
            (name.to_string(), (self.live * share * jitter).round())
        })
        .collect();
        (self.live, realms)
    }
}

// ---------------------------------------------------------------- capture ---

/// The captures the site embeds: the wide one and the phone-sized reflow.
const CAPTURES: [(u16, u16); 2] = [(132, 52), (74, 58)];
/// Fixed, so `make capture` produces the same numbers every run and a regenerated
/// site is a diff of what actually changed rather than of fresh demo jitter.
const CAPTURE_SEED: u64 = 0x0a0e_c4af;

/// The demo dashboard as the site shows it: eased fully into place, so nothing
/// is captured half-animated.
fn capture_dash() -> Dash {
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::seed_from_u64(CAPTURE_SEED);

    let mut synthetic = Synthetic::new();
    let snapshot = synthetic.report(&mut rng);
    let (live, realms) = synthetic.live(&mut rng);
    let mut dash = Dash::new(
        "Contoso Labs (demo)".to_string(),
        7,
        snapshot,
        live,
        realms,
        Duration::from_secs(30),
        Duration::from_secs(5),
    );

    // Run the realtime poll forward for a while before capturing. A cold start
    // has an empty trace and an empty feed, so capturing one shows the realtime
    // panel with a flat line and "quiet out there" — the two things it exists to
    // disprove.
    for tick in 0..HISTORY {
        if tick % 8 == 0 {
            let (live, realms) = synthetic.live(&mut rng);
            dash.apply_live(live, realms);
        }
        dash.step(FRAME.as_secs_f64());
        dash.trace();
    }
    // Settle whatever is still easing, so nothing is caught mid-flight.
    for _ in 0..120 {
        dash.step(FRAME.as_secs_f64());
    }
    // The footer stamps the wall clock, which would otherwise be the one thing
    // that differs every time the captures are regenerated.
    dash.updated = "17:24:53".to_string();
    // The site shows the dashboard an Anacrafter sees: the gold star beside the
    // brand and the thank-you in place of the ask. It is the look the plan is
    // selling, so the page that sells it should be the one wearing it. Not
    // `demo`, which would splice the preview wording into the capture.
    dash.supporter = true;
    dash
}

/// One layer of a capture. The site paints cell backgrounds and glyphs as two
/// stacked plates, the order a terminal composites in — see the note on `.dash`
/// in `docs/index.html` for why a single plate cannot hold both.
fn plate(buffer: &Buffer, background: bool) -> String {
    let area = *buffer.area();
    let mut out = String::new();

    for y in 0..area.height {
        if y > 0 {
            out.push('\n');
        }
        // Runs of cells sharing a color collapse into one tag, or the page
        // would carry a span per cell and weigh several megabytes.
        let mut run = String::new();
        let mut key: Option<(Option<String>, bool)> = None;

        let flush = |out: &mut String, run: &mut String, key: &Option<(Option<String>, bool)>| {
            if run.is_empty() {
                return;
            }
            let text = escape(run, !background);
            match key {
                Some((Some(hex), bold)) => {
                    let property = if background { "background" } else { "color" };
                    let weight = if *bold { ";font-weight:700" } else { "" };
                    out.push_str(&format!("<b style=\"{property}:{hex}{weight}\">{text}</b>"));
                }
                // A cell the theme never colored: the plate leaves it bare.
                _ => out.push_str(&text),
            }
            run.clear();
        };

        for x in 0..area.width {
            let cell = &buffer[(x, y)];
            let color = if background { cell.bg } else { cell.fg };
            let bold = !background && cell.modifier.contains(Modifier::BOLD);
            let next = (hex(color), bold);
            if key.as_ref() != Some(&next) {
                flush(&mut out, &mut run, &key);
                key = Some(next);
            }
            run.push_str(cell.symbol());
        }
        flush(&mut out, &mut run, &key);
    }

    out
}

/// `Color::Rgb` is all the palettes use, so anything else is a cell nobody
/// styled and the plate leaves it to the page's own background.
fn hex(color: Color) -> Option<String> {
    match color {
        Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
        _ => None,
    }
}

/// HTML-escapes a run, and pins the pickaxe's width on the glyph plate: U+26CF
/// is absent from JetBrains Mono and falls back to an emoji wider than its cell,
/// which would shift everything after it out of the grid.
fn escape(text: &str, pin_wide: bool) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    if pin_wide {
        escaped.replace(
            glyph::PICKAXE,
            &format!(
                "<i class=\"wide\" style=\"width:0.6em\">{}</i>",
                glyph::PICKAXE
            ),
        )
    } else {
        escaped
    }
}

/// Renders the demo dashboard for every palette, at both captures, as the
/// two-plate HTML `docs/index.html` embeds between its capture markers.
///
/// The site used to carry these by hand, which is how it came to advertise a
/// panel the dashboard had stopped drawing and a palette it never shipped.
pub fn capture() -> Result<String> {
    use ratatui::backend::TestBackend;

    let mut out = String::new();

    for (index, (width, height)) in CAPTURES.iter().enumerate() {
        let class = if index == 0 { "wide" } else { "narrow" };
        out.push_str(&format!("<div class=\"plate {class}\">"));

        for (nth, palette) in theme::THEMES.iter().enumerate() {
            if !theme::select(palette.name) {
                anyhow::bail!("no palette named {}", palette.name);
            }
            let dash = capture_dash();

            let mut terminal = Terminal::new(TestBackend::new(*width, *height))?;
            terminal.draw(|frame| draw(frame, &dash))?;
            let buffer = terminal.backend().buffer();

            // Only the first is shown; the tabs unhide the others.
            let hidden = if nth == 0 { "" } else { " hidden" };
            out.push_str(&format!(
                "<pre class=\"dash\" data-theme=\"{}\"{hidden}>\
                 <span class=\"lyr bgl\" aria-hidden=\"true\">{}</span>\
                 <span class=\"lyr fgl\">{}</span></pre>",
                palette.name,
                plate(buffer, true),
                plate(buffer, false),
            ));
        }

        out.push_str("</div>\n");
    }

    Ok(out)
}

// ------------------------------------------------------------------- draw ---

/// What the dashboard shows instead of itself when the terminal is too small.
///
/// Names both numbers and marks only the one that is short, so the fix is
/// obvious without having to compare four figures. Nothing animates: this is a
/// screen you are meant to read once and leave, and a pulsing dot on it would
/// suggest the dashboard is still working on something.
fn too_small(area: Rect) -> Paragraph<'static> {
    let short = |have: u16, need: u16| {
        if have < need {
            Style::default()
                .fg(ore::redstone())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::fade(theme::sage(), 0.3))
        }
    };

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("  {} ANACRAFT ", glyph::PICKAXE),
                Style::default()
                    .fg(ore::grass())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                theme::say(
                    "· the shaft is too tight to work in",
                    "· not enough room to draw",
                ),
                Style::default().fg(ore::stone()),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  have  ", Style::default().fg(ore::stone())),
            Span::styled(format!("{:>4}", area.width), short(area.width, MIN_COLS)),
            Span::styled(" × ", Style::default().fg(ore::netherite())),
            Span::styled(format!("{:<4}", area.height), short(area.height, MIN_ROWS)),
        ]),
        Line::from(vec![
            Span::styled("  needs ", Style::default().fg(ore::stone())),
            Span::styled(
                format!("{MIN_COLS:>4}"),
                Style::default().fg(theme::accent()),
            ),
            Span::styled(" × ", Style::default().fg(ore::netherite())),
            Span::styled(
                format!("{MIN_ROWS:<4}"),
                Style::default().fg(theme::accent()),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  resize the terminal · q to quit",
            Style::default().fg(theme::fade(theme::sage(), 0.3)),
        )),
    ];

    Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ore::netherite()))
            .style(Style::default().bg(theme::bg_lift())),
    )
}

/// Whether this install is an Anacrafter, and how to become one if not.
///
/// A box of its own rather than a slot in the footer hotbar. The footer is
/// where the keybinds live and it runs out of room on a narrow terminal, so an
/// ask parked at the end of it is the first thing to disappear on exactly the
/// setups least likely to have seen it before. This is the one line in the
/// dashboard that pays for the rest, so it gets space that nothing else can
/// take.
///
/// Both states use the same box, because the answer to "am I an Anacrafter" is
/// worth stating either way — one line is a thank-you, the other is an ask.
fn supporter_box(dash: &Dash, width: u16) -> Paragraph<'static> {
    // Its borders, and one cell held back so the line never ends flush
    // against the closing one.
    let room = width.saturating_sub(3) as usize;
    let star = Span::styled(
        format!("  {} ", glyph::STAR),
        Style::default()
            .fg(ore::gold())
            .add_modifier(Modifier::BOLD),
    );

    let line = if dash.supporter {
        let spans = vec![
            star,
            Span::styled(
                "ANACRAFTER",
                Style::default()
                    .fg(ore::gold())
                    .add_modifier(Modifier::BOLD),
            ),
            // The number rides the word, not a corner of its own: it is part
            // of the name here, the way a jersey number is. Padded to three so
            // the first hundred line up under each other, and unbold so the
            // word still leads.
            Span::styled(
                match dash.founder {
                    Some(number) => format!(" #{number:03}"),
                    None => String::new(),
                },
                Style::default().fg(ore::gold()),
            ),
            Span::styled(
                // Said out loud in the demo: nobody should read a synthetic
                // dashboard as proof they already subscribed.
                if dash.demo {
                    "  ·  preview  ·  press s to switch back".to_string()
                } else {
                    format!("  ·  {}", dash.supporter_line)
                },
                Style::default().fg(ore::stone()),
            ),
            Span::styled(
                // The plan this machine is on, when the box has one to name.
                // Matches `dash.supporter` rather than the config, so the demo
                // preview reads the way a real dashboard would.
                match dash.tier {
                    Some(plan) if !dash.demo => format!("  ·  on {}", plan.label()),
                    _ => String::new(),
                },
                Style::default().fg(ore::stone()),
            ),
        ];
        // The badge is the point of the box; the line beside it is the gloss.
        // A narrow terminal gives up the gloss rather than handing the border
        // half a word to cut — "none of the invo".
        Line::from(fits(spans, room, 3))
    } else {
        let mut spans = vec![
            star,
            Span::styled(
                "not an Anacrafter yet",
                Style::default().fg(theme::accent()),
            ),
            Span::styled("  ·  run ", Style::default().fg(ore::stone())),
            Span::styled(
                "craft subscribe",
                Style::default()
                    .fg(ore::gold())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ·  {}", crate::price_line()),
                Style::default().fg(ore::stone()),
            ),
        ];
        // The ask keeps its wording; the demo just adds the way to see what is
        // being asked for.
        if dash.demo {
            spans.push(Span::styled(
                "  ·  press ",
                Style::default().fg(ore::stone()),
            ));
            spans.push(Span::styled(
                "s",
                Style::default()
                    .fg(ore::gold())
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                " to preview",
                Style::default().fg(ore::stone()),
            ));
        }
        Line::from(fits(spans, room, 4))
    };

    Paragraph::new(line).block(
        Block::default()
            .borders(Borders::ALL)
            // Gold when there is something to ask for, quiet once there isn't:
            // a permanent box should stop drawing the eye after it has been
            // answered.
            .border_style(Style::default().fg(if dash.supporter {
                ore::netherite()
            } else {
                theme::fade(ore::gold(), 0.45)
            }))
            .style(Style::default().bg(theme::bg_lift()))
            // Which build this is, riding the border of the one box that is
            // always on screen and has room to spare.
            //
            // On a border rather than in a panel because it is a fact somebody
            // looks up twice a year — when a release note mentions something
            // they do not have, and when they are reporting that it broke.
            // Every row of content is worth more than that, and the footer is
            // already over budget at eighty columns.
            .title_top(version_tag()),
    )
}

/// The running build, for the corner of a border.
fn version_tag() -> Line<'static> {
    Line::from(Span::styled(
        format!(" v{} ", env!("CARGO_PKG_VERSION")),
        Style::default().fg(ore::stone()),
    ))
    .right_aligned()
}

/// Which of the left column's panels get rows, and how many they are pinned to,
/// in the order they sit down the column.
///
/// Lifted out of `draw` so the rule can be tested as the rule rather than as a
/// copy of it written out again in a test.
///
/// Rows are claimed in priority order — events, then the map, then vitals — and
/// a panel that cannot get its minimum is left out rather than squeezed into a
/// few rows, the same rule the right column follows. Events leads and never
/// gives way: it is the headline chart the dashboard is named for, and a column
/// that keeps a table of figures by compressing that chart into four rows has
/// its priorities backwards.
///
/// The map comes next, ahead of the figures, because it is the one panel here
/// you cannot get out of `craft overview`. Every number in the vitals is a
/// one-shot report away; the world is only ever drawn here. So on a column too
/// short for both, the map is what stays.
///
/// It rarely comes to that, because what vitals claims is the least it can be
/// drawn in rather than the height it would like. Sitting at the bottom, it is
/// handed every row the others leave and grows back through its three densities
/// as they do — which is why all three fit at 50 rows where reserving the full
/// twenty for the vitals needed 62.
fn left_column(panels: &Panels, height: u16) -> Vec<(Stack, u16)> {
    let mut budget = height;
    let mut stack: Vec<(Stack, u16)> = Vec::new();

    // A box costs its own rows plus one for the gutter above it — and the first
    // box in the column has nothing above it, so it costs only its rows.
    // Charging the gutter unconditionally is how events came to be rejected at
    // exactly EVENTS_ROWS while the shorter map box slipped in underneath it,
    // which inverts the very priority this order sets.
    let mut cost = |needs: u16, placed: bool| -> bool {
        let needs = needs + u16::from(placed);
        let fits = budget >= needs;
        if fits {
            budget -= needs;
        }
        fits
    };

    if panels.events && cost(EVENTS_ROWS, false) {
        stack.push((Stack::Events, EVENTS_ROWS));
    }
    if panels.map && cost(MAP_ROWS, !stack.is_empty()) {
        stack.push((Stack::Map, MAP_ROWS));
    }
    if panels.vitals && cost(VITALS_MIN_ROWS, !stack.is_empty()) {
        stack.push((Stack::Vitals, VITALS_MIN_ROWS));
    }

    // With the figures off there is nothing below the map that can use a row,
    // so it takes as many as it can draw and the chart above it takes the rest.
    // Only then: while the vitals are up those rows are what brings their bars
    // and gaps back, and a map cannot spend a row past `MAP_MAX_ROWS` at all —
    // giving it three of theirs would buy dead ground with a density.
    if !panels.vitals {
        if let Some(slot) = stack
            .iter_mut()
            .find(|(panel, _)| matches!(panel, Stack::Map))
        {
            // Not taken off `budget`: the rows left after this pass are handed
            // out by `draw`, from the claims this returns.
            slot.1 += budget.min(MAP_MAX_ROWS - MAP_ROWS);
        }
    }

    // Who gets rows is one question; where they sit is another. Sort back into
    // column order so changing the priority above never reshuffles the
    // dashboard — the map stays between the chart and the figures whenever all
    // three are up.
    stack.sort_by_key(|(panel, _)| match panel {
        Stack::Events => 0,
        Stack::Map => 1,
        Stack::Vitals => 2,
    });
    stack
}

fn draw(frame: &mut Frame, dash: &Dash) {
    let area = frame.area();

    // Paint the Osaka Jade ground first: the darkest shade in the palette, so
    // the dashboard looks the same against any terminal background and the
    // panels above it have something to lift off.
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::ink()).fg(theme::fg())),
        area,
    );

    if area.width < MIN_COLS || area.height < MIN_ROWS {
        frame.render_widget(too_small(area), area);
        return;
    }

    // Every layout below leaves a one-cell gutter. That gap is the ink ground
    // showing through, which is what makes the panels read as floating on it
    // rather than as one tiled surface.
    // The grid's shape is the *terminal's* shape, not the body's: the header,
    // box and footer between them ate the rows that would keep the body's
    // aspect honest, so measuring it here would call a 4:3 window "wide" and
    // never go two-across. The whole frame — half as tall again as it is wide
    // (16:9 and beyond) runs three across; 4:3 and squarer runs two.
    let wide = (area.width as u32) * 2 >= (area.height as u32) * 3;

    // An 80-column terminal has nothing to spare: the vitals rows clip their
    // delta first, so at that width the dashboard drops its outer margin and
    // widens the left panel rather than losing the numbers.
    let narrow = area.width < 100;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(SUPPORTER_ROWS),
            Constraint::Length(3),
        ])
        .margin(if narrow { 0 } else { 1 })
        .spacing(1)
        .split(area);

    frame.render_widget(header(dash, chunks[0].width), chunks[0]);
    body(frame, dash, chunks[1], narrow, wide);
    frame.render_widget(supporter_box(dash, chunks[2].width), chunks[2]);
    frame.render_widget(footer(dash, chunks[3].width), chunks[3]);

    // The confirmation sits above the help, because it is a question waiting
    // on an answer and the help is not.
    if let Some(target) = &dash.forget {
        forget_overlay(frame, area, target);
    } else if let Some(wired) = &dash.wired {
        wired_overlay(frame, area, wired);
    } else if dash.help {
        help_overlay(frame, area, dash.demo);
    }
}

/// Lays out whichever panels are switched on. A hidden panel doesn't leave a
/// hole — the remaining ones take its space, so `2` on a wide terminal turns
/// the dashboard into vitals beside a full-height chunk list.
fn body(frame: &mut Frame, dash: &Dash, area: Rect, narrow: bool, wide: bool) {
    if !dash.panels.any() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  every panel is hidden — 1, 2 or 3 brings one back, ? lists the keys",
                Style::default().fg(ore::stone()),
            )))
            .block(framed("ANACRAFT", "", ore::stone())),
            area,
        );
        return;
    }

    // The grid picks its shape from the body's aspect: a body half as tall
    // again as it is wide — 16:9 and beyond — runs the tiles three across,
    // two rows of them. A squarer body, 4:3 and down, runs two across so the
    // taller body earns a third row. Only a body more than twice as tall as
    // it is wide falls back to the columns, where tiles would be too narrow
    // to read.
    if area.height as u32 > (area.width as u32) * 2 {
        columns(frame, dash, area, narrow);
    } else {
        grid(frame, dash, area, wide);
    }
}

/// Which panel occupies a tile in the body grid, in key order — the order the
/// keys list them and the order a reader scans a page. The tiles fill the
/// grid in that same order, so the dashboard reads the way the keys list it:
/// the first handful of switched-on panels up front, then the rest of the
/// queue waiting behind them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tile {
    Events,
    Live,
    Map,
    Chunks,
    Vitals,
    RealmsRanked,
    Trend,
    Portals,
}

impl Tile {
    fn on(&self, panels: &Panels) -> bool {
        match self {
            Tile::Events => panels.events,
            Tile::Live => panels.live,
            Tile::Map => panels.map,
            Tile::Chunks => panels.chunks,
            Tile::Vitals => panels.vitals,
            Tile::RealmsRanked => panels.realms_ranked,
            Tile::Trend => panels.trend,
            Tile::Portals => panels.portals,
        }
    }

    /// Where the tile sits in `Dash::joined`, which is key order: 1 to 8.
    fn index(self) -> usize {
        match self {
            Tile::Events => 0,
            Tile::Live => 1,
            Tile::Map => 2,
            Tile::Chunks => 3,
            Tile::Vitals => 4,
            Tile::RealmsRanked => 5,
            Tile::Trend => 6,
            Tile::Portals => 7,
        }
    }

    /// The switch behind the tile, to flip.
    fn switch(self, panels: &mut Panels) -> &mut bool {
        match self {
            Tile::Events => &mut panels.events,
            Tile::Live => &mut panels.live,
            Tile::Map => &mut panels.map,
            Tile::Chunks => &mut panels.chunks,
            Tile::Vitals => &mut panels.vitals,
            Tile::RealmsRanked => &mut panels.realms_ranked,
            Tile::Trend => &mut panels.trend,
            Tile::Portals => &mut panels.portals,
        }
    }

    /// The share of its row the tile asks for. The chart and the map read
    /// across: a time series and a world map both turn width into detail,
    /// where a list turns it into trailing space beside the same numbers.
    fn weight(&self) -> u32 {
        match self {
            Tile::Events | Tile::Map => 5,
            _ => 4,
        }
    }

    /// The least the tile can be and still be that panel — all its borders.
    fn min_rows(&self) -> u16 {
        match self {
            Tile::Events => EVENTS_ROWS,
            Tile::Live => LIVE_ROWS,
            Tile::Map => MAP_ROWS,
            Tile::Chunks => CHUNKS_ROWS,
            Tile::Vitals => VITALS_MIN_ROWS,
            Tile::RealmsRanked => REALMS_RANKED_ROWS,
            Tile::Trend => TREND_ROWS,
            Tile::Portals => PORTALS_ROWS,
        }
    }

    /// The most rows a tile can put to use. `u16::MAX` marks a panel that
    /// grows — the lists show longer lists, the chart just gets taller.
    fn max_rows(&self) -> u16 {
        match self {
            Tile::Map => MAP_MAX_ROWS,
            Tile::Vitals => VITALS_ROWS,
            Tile::Events | Tile::Chunks | Tile::RealmsRanked | Tile::Portals => u16::MAX,
            _ => self.min_rows(),
        }
    }
}

/// The body as a grid of tiles, filled in the order the panels come. The
/// terminal's aspect names the widest a row may be — three tiles across on a
/// body half as tall again as it is wide (16:9 and beyond), two on 4:3 and
/// squarer — and the body's own width can name fewer still. The panels fill
/// the cells top row first, left to right, in the order they were switched
/// on — key order until something is pressed — and every one that is on gets
/// a cell: switching one on takes another row and pushes the rest of the
/// board down to pay for it.
///
/// The board splits the way a tiling manager does rather than filling the
/// widest row it is allowed. One panel takes the whole body; the second
/// halves it; the third drops a full-width row beneath the pair instead of
/// wedging a third column in beside them; the fourth joins that row and makes
/// a 2×2. Only from the fifth does a row go three across, and only where the
/// body is wide enough to grant it.
///
/// However they fall, the rows carry within one tile of each other — no row
/// of three over a lone stretched fourth. Switching one off re-tiles the rest
/// over the space it gave up, the way closing a window in a tiling manager
/// does.
///
/// A tile that a row cannot fit is dropped from the last row up: the grid
/// gives up the panel, never the dashboard. Spare rows go to the panels that
/// can spend them — the vitals first, which turn them into bars and gaps,
/// then the lists and the chart. A row grows only as far as *every* tile in
/// it can spend the extra row; the least-fillable tile sets its ceiling, so
/// a row's height does not swing when a neighbour joins it.
fn grid(frame: &mut Frame, dash: &Dash, area: Rect, wide: bool) {
    // The terminal sets the ceiling on a row's width: at most three across
    // when it is half again as wide as it is tall, two when it is squarer.
    // How many of those a row actually takes is the tile count's call, below.
    //
    // The aspect only ever asks for columns — the body's width is what grants
    // them. A 60-column terminal is wider than it is tall and so asked for
    // three, which left tiles seventeen cells across with their headers cut
    // mid-word; the grid now drops to as many columns as `MIN_TILE_COLS` will
    // actually fit, down to a single stack. However the columns fall, the rows
    // a narrower body runs make up for the columns it gave up.
    let fits = (1..=3u16)
        .rev()
        .find(|n| n * MIN_TILE_COLS + (n - 1) <= area.width)
        .unwrap_or(1) as usize;
    let most = if wide { 3 } else { 2 }.min(fits);

    // The panels the board could hold, in key order — which is the order an
    // untouched board draws them in. A switched-off panel is skipped; the
    // cells that stay left spread across their row's width, so an empty cell
    // widens its neighbours instead of leaving a hole.
    //
    // Every panel that is on gets a cell. There used to be a ceiling of six,
    // which is why pressing 8 on a full board did nothing at all: the panel
    // came on and found nowhere to be drawn. A tile makes room for itself
    // instead — the board takes another row and everything already on it
    // gives up the height to pay for it. Only a body with no room left for
    // the tile's own borders drops one, from the bottom row up.
    let order = [
        Tile::Events,
        Tile::Live,
        Tile::Map,
        Tile::Chunks,
        Tile::Vitals,
        Tile::RealmsRanked,
        Tile::Trend,
        Tile::Portals,
    ];
    let mut on: Vec<Tile> = order
        .iter()
        .copied()
        .filter(|tile| tile.on(&dash.panels))
        .collect();
    // Key order is only where the board starts. What fixes a tile's place is
    // when it was switched on, so a panel brought back joins at the back
    // rather than shuffling into the middle and moving everything after it.
    on.sort_by_key(|tile| dash.joined[tile.index()]);
    if on.is_empty() {
        return;
    }

    // `most` is the widest a row may be, not the width every row must be. The
    // board used to take it as the target, which is why three panels on a
    // wide terminal came out as three narrow strips across the top instead of
    // a pair over a full-width row. It splits to the count instead — the
    // smallest square that holds every tile — and the terminal's ceiling caps
    // that. Three panels ask for two and get a pair over a full-width row;
    // four fill the 2×2; five ask for three and get it where the body is wide
    // enough. The rows then carry within one tile of each other, the fuller
    // ones first, the way a tiling window manager splits its space.
    let across = (1..=most).find(|n| n * n >= on.len()).unwrap_or(most);
    let rows_deep = on.len().div_ceil(across);
    let per = on.len() / rows_deep;
    let wider = on.len() % rows_deep;

    let mut rows: Vec<Vec<Tile>> = Vec::with_capacity(rows_deep);
    let mut queue = on.as_slice();
    for depth in 0..rows_deep {
        let (row, rest) = queue.split_at(per + usize::from(depth < wider));
        rows.push(row.to_vec());
        queue = rest;
    }

    // Each row's height is its tallest tile at minimum. Dropped tiles come
    // off the bottom row until the whole grid fits the screen.
    let mut row_min: Vec<u16> = rows
        .iter()
        .map(|row| row.iter().map(Tile::min_rows).max().unwrap_or(0))
        .collect();
    loop {
        let gutter = rows.len().saturating_sub(1) as u16;
        if gutter + row_min.iter().sum::<u16>() <= area.height {
            break;
        }
        let last = rows.len() - 1;
        rows[last].pop();
        if rows[last].is_empty() {
            rows.pop();
            row_min.pop();
        } else {
            row_min[last] = rows[last].iter().map(Tile::min_rows).max().unwrap_or(0);
        }
        if rows.is_empty() {
            return;
        }
    }

    // Leftover rows go to the first panel in priority order that can spend
    // them: the figures turn rows into bars and gaps, the map has a ceiling,
    // and the lists and the chart grow without one. A row grows only as far
    // as *every* tile in it can spend the extra row — the least-fillable tile
    // sets the ceiling, or the boxes with short content would show a ground
    // of their own between their borders.
    let row_cap: Vec<u16> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(Tile::max_rows)
                .min()
                .unwrap_or(0)
                .min(VITALS_ROWS)
        })
        .collect();
    let mut slack =
        area.height - (rows.len().saturating_sub(1) as u16 + row_min.iter().sum::<u16>());
    let mut grown = row_min.clone();
    for tile in [
        Tile::Vitals,
        Tile::Map,
        Tile::Chunks,
        Tile::RealmsRanked,
        Tile::Events,
    ] {
        let Some(spot) = rows.iter().position(|row| row.contains(&tile)) else {
            continue;
        };
        let headroom = tile
            .max_rows()
            .saturating_sub(grown[spot])
            .min(row_cap[spot].saturating_sub(grown[spot]));
        let take = slack.min(headroom);
        grown[spot] += take;
        slack -= take;
        if slack == 0 {
            break;
        }
    }

    // Whatever the panels would not spend is spread over the rows regardless.
    // The capped pass above stops as soon as no tile can *use* another row,
    // which left the grid ending short of the body and a band of bare ground
    // above the supporter box — four rows of it on a 132-column terminal, the
    // size the site is captured at. A panel carrying a little room under its
    // last line reads as a panel; a gap between the panels and the footer
    // reads as the dashboard having stopped drawing. The grid ends where the
    // body ends.
    if slack > 0 {
        let count = grown.len() as u16;
        let each = slack / count;
        let extra = slack % count;
        for (i, row) in grown.iter_mut().enumerate() {
            *row += each + u16::from((i as u16) < extra);
        }
    }

    let constraints: Vec<Constraint> = grown.iter().map(|rows| Constraint::Length(*rows)).collect();
    let row_rects = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .spacing(1)
        .split(area);

    for (row, rect) in rows.iter().zip(row_rects.iter()) {
        // The chart and the map take the wider share of their row — but only
        // where the row can pay for it. The extra a wide tile gains comes off
        // its neighbour, and a neighbour pushed under `MIN_TILE_COLS` loses
        // its labels, which costs the row more than the map gains. So a phone-
        // sized row goes back to equal shares and everything stays readable.
        let weights: Vec<u32> = row.iter().map(Tile::weight).collect();
        let asked: u32 = weights.iter().sum();
        let leanest = weights.iter().copied().min().unwrap_or(1);
        let span = rect.width.saturating_sub(row.len() as u16 - 1) as u32;
        let constraints: Vec<Constraint> = if span * leanest / asked >= MIN_TILE_COLS as u32 {
            weights
                .iter()
                .map(|weight| Constraint::Ratio(*weight, asked))
                .collect()
        } else {
            std::iter::repeat(Constraint::Ratio(1, row.len() as u32))
                .take(row.len())
                .collect()
        };

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
            .spacing(1)
            .split(*rect);

        for (tile, cell) in row.iter().zip(cols.iter()) {
            match tile {
                Tile::Events => frame.render_widget(events_panel(dash, cell.width), *cell),
                Tile::Live => frame.render_widget(live_panel(dash, cell.width), *cell),
                Tile::Map => frame.render_widget(map_panel(dash, cell.width, cell.height), *cell),
                Tile::Chunks => {
                    frame.render_widget(pages_panel(dash, cell.width, cell.height), *cell)
                }
                Tile::Vitals => {
                    frame.render_widget(metrics_panel(dash, cell.width, cell.height), *cell)
                }
                Tile::RealmsRanked => {
                    frame.render_widget(realms_ranked_panel(dash, cell.width, cell.height), *cell)
                }
                Tile::Trend => frame.render_widget(trend_panel(dash, cell.width), *cell),
                Tile::Portals => {
                    frame.render_widget(portals_panel(dash, cell.width, cell.height), *cell)
                }
            }
        }
    }
}

/// The two stacked columns, for terminals too narrow for a tile grid.
fn columns(frame: &mut Frame, dash: &Dash, area: Rect, narrow: bool) {
    // Each column is drawn if anything in it is on, and whichever is alone
    // takes the whole body. Hiding a panel hides that panel: the ones left in
    // its column take the rows it was using, in the order they sit in.
    let right_any = dash.panels.right_any();
    let left_any = dash.panels.left_any();
    let (left, right) = if left_any && right_any {
        let split = if narrow { (63, 37) } else { (56, 44) };
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(split.0),
                Constraint::Percentage(split.1),
            ])
            .spacing(1)
            .split(area);
        (Some(cols[0]), Some(cols[1]))
    } else if left_any {
        (Some(area), None)
    } else {
        (None, Some(area))
    };

    if let Some(rect) = left {
        let stack = left_column(&dash.panels, rect.height);

        // One box takes the rows nobody claimed, so the column does not trail
        // off into dead ground while the one beside it is full. Everything
        // else is pinned to the height it asked for — sharing the slack out
        // would stretch every box a little instead.
        //
        // Which box gets it is the question. It used to be whichever sat at
        // the bottom, which is the vitals when they fit and the *map* when
        // they do not — and the map cannot spend a row past `MAP_MAX_ROWS`,
        // so on a terminal too short for the vitals the slack went to the one
        // box guaranteed to draw nothing with it. So it goes to a panel that
        // grows: the vitals first, which turn spare rows into bars and then
        // into gaps between them, and the chart second, which just gets
        // taller. The map takes it only when it is the whole column, and even
        // then no further than it can draw.
        let stretch = stack
            .iter()
            .position(|(panel, _)| matches!(panel, Stack::Vitals))
            .or_else(|| {
                stack
                    .iter()
                    .position(|(panel, _)| matches!(panel, Stack::Events))
            });
        let constraints: Vec<Constraint> = stack
            .iter()
            .enumerate()
            .map(|(i, (panel, needs))| match (stretch, panel) {
                (Some(at), _) if at == i => Constraint::Min(*needs),
                // Nothing else to hand it to. The box stops where the map
                // does and the ground shows below it, which reads as a panel
                // that ended rather than one that was left half empty.
                (None, Stack::Map) => Constraint::Max(MAP_MAX_ROWS),
                (None, _) if i == stack.len().saturating_sub(1) => Constraint::Min(*needs),
                _ => Constraint::Length(*needs),
            })
            .collect();

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .spacing(1)
            .split(rect);

        for ((panel, _), area) in stack.into_iter().zip(rows.iter()) {
            match panel {
                Stack::Map => frame.render_widget(map_panel(dash, area.width, area.height), *area),
                Stack::Events => frame.render_widget(events_panel(dash, area.width), *area),
                Stack::Vitals => {
                    frame.render_widget(metrics_panel(dash, area.width, area.height), *area)
                }
            }
        }
    }

    let Some(rect) = right else { return };

    // The right column stacks whichever of its three panels are on, daily chart
    // at the bottom. Rows are handed out in priority order and a panel that
    // cannot get its minimum is left out entirely — a two-row box with its
    // contents squeezed away is worse than no box.
    let mut budget = rect.height;
    let mut panels = Vec::new();
    for (on, column, needs) in [
        (dash.panels.live, Column::Live, LIVE_ROWS),
        (dash.panels.chunks, Column::Chunks, CHUNKS_ROWS),
        // Ahead of the ranked realms deliberately. The map above already says
        // which countries, so that list is the one panel here with a second
        // answer to the same question — where this one is the only answer to
        // "who sent them".
        (dash.panels.portals, Column::Portals, PORTALS_ROWS),
        (
            dash.panels.realms_ranked,
            Column::RealmsRanked,
            REALMS_RANKED_ROWS,
        ),
        (dash.panels.trend, Column::Trend, TREND_ROWS),
    ] {
        let gap = u16::from(!panels.is_empty());
        if on && budget >= needs + gap {
            budget -= needs + gap;
            panels.push((column, needs));
        }
    }
    if panels.is_empty() {
        return;
    }

    // Leftover rows go to the chunk list or realms ranked, which are the panels
    // that can use them; failing that, to whatever is first.
    let stretch = panels
        .iter()
        .position(|(column, _)| matches!(column, Column::Chunks | Column::RealmsRanked))
        .unwrap_or(0);
    let constraints: Vec<Constraint> = panels
        .iter()
        .enumerate()
        .map(|(i, (_, needs))| {
            if i == stretch {
                Constraint::Min(*needs)
            } else {
                Constraint::Length(*needs)
            }
        })
        .collect();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .spacing(1)
        .split(rect);
    let panels: Vec<Column> = panels.into_iter().map(|(column, _)| column).collect();

    for (panel, area) in panels.into_iter().zip(rows.iter()) {
        let widget = match panel {
            Column::Live => live_panel(dash, area.width),
            Column::Chunks => pages_panel(dash, area.width, area.height),
            Column::RealmsRanked => realms_ranked_panel(dash, area.width, area.height),
            Column::Portals => portals_panel(dash, area.width, area.height),
            Column::Trend => trend_panel(dash, area.width),
        };
        frame.render_widget(widget, *area);
    }
}

/// The key list, centered over whatever is on screen.
/// Shift+D's question.
///
/// The line that matters is the one saying the property stays in Google. A
/// confirmation box in a dashboard is where somebody assumes the opposite, and
/// they would only find out they were wrong much later.
fn forget_overlay(frame: &mut Frame, area: Rect, target: &Forget) {
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("  {}  ", target.name),
                Style::default()
                    .fg(ore::diamond())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("({})", target.id),
                Style::default().fg(theme::sage()),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  drops out of the tab rotation here",
            Style::default().fg(theme::fg()),
        )),
        Line::from(vec![
            Span::styled(
                "  ! ",
                Style::default()
                    .fg(ore::redstone())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "stays in Google, with its data",
                Style::default()
                    .fg(theme::fg())
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  delete it in the Analytics console:",
            Style::default().fg(theme::sage()),
        )),
        Line::from(Span::styled(
            "  Property details → Move to Trash Can",
            Style::default().fg(theme::sage()),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  y",
                Style::default()
                    .fg(ore::gold())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" forget    ", Style::default().fg(theme::fg())),
            Span::styled(
                "esc",
                Style::default()
                    .fg(ore::gold())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" keep it", Style::default().fg(theme::fg())),
        ]),
        Line::from(""),
    ];

    let width = 44.min(area.width.saturating_sub(4));
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(framed("FORGET PROPERTY", "", ore::redstone())),
        rect,
    );
}

fn help_overlay(frame: &mut Frame, area: Rect, demo: bool) {
    let mut keys = vec![
        ("q / Esc", "quit"),
        ("r", "refresh now"),
        ("^1 / 1", "events panel"),
        ("^2 / 2", "right now panel"),
        ("^3 / 3", "countries map"),
        ("^4 / 4", "top pages panel"),
        ("^5 / 5", "vitals panel"),
        ("^6 / 6", "top countries"),
        ("^7 / 7", "daily users"),
        (
            "^8 / 8",
            theme::say("portals — who sent them", "traffic sources"),
        ),
        ("c", "copy the MCP connector"),
        ("t", "next theme"),
        ("b", "boring - plain GA4 names"),
        ("tab", "next property"),
        ("? / h", "this list"),
    ];
    // Only listed where they do something.
    if demo {
        keys.push(("s", "preview Anacrafter"));
    } else {
        keys.push(("shift+D", "forget property"));
    }

    let width = 40.min(area.width.saturating_sub(4));
    let height = (keys.len() as u16 + 3).min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let mut lines = vec![Line::from("")];
    for (key, what) in keys {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {key:<9}"),
                Style::default()
                    .fg(ore::gold())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(what.to_string(), Style::default().fg(theme::fg())),
        ]));
    }

    // `Clear` first, or the panel underneath bleeds through the overlay.
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(framed("KEYS", "", ore::gold())),
        rect,
    );
}

/// What `c` copied, wide enough to read the line it copied.
///
/// Wider than the other two overlays on purpose. The command is a hundred and
/// forty characters and it is the thing somebody came here for — wrapped over
/// four lines it is still selectable with a mouse, which is the fallback for
/// every terminal that quietly declines OSC 52.
fn wired_overlay(frame: &mut Frame, area: Rect, wired: &Wired) {
    let half = |text: String| {
        Line::from(Span::styled(
            format!("  {text}"),
            Style::default().fg(theme::sage()),
        ))
    };

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", wired.property),
            Style::default()
                .fg(ore::diamond())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    match &wired.trouble {
        Some(trouble) => {
            lines.push(Line::from(Span::styled(
                format!("  {trouble}"),
                Style::default().fg(ore::redstone()),
            )));
        }
        None => {
            lines.push(Line::from(Span::styled(
                "  on the clipboard, if this terminal takes OSC 52:",
                Style::default().fg(theme::sage()),
            )));
            lines.push(Line::from(""));
            for chunk in wrapped(&wired.link, 60) {
                lines.push(Line::from(Span::styled(
                    format!("  {chunk}"),
                    Style::default().fg(theme::fg()),
                )));
            }
            lines.push(Line::from(""));
            lines.push(half("paste it into the client's URL field, or:".into()));
            for chunk in wrapped(&wired.command, 60) {
                lines.push(half(chunk));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  `craft serve` has to be running for it to answer",
                Style::default().fg(theme::sage()),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            "  any key",
            Style::default()
                .fg(ore::gold())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" closes this", Style::default().fg(theme::fg())),
    ]));
    lines.push(Line::from(""));

    let width = 68.min(area.width.saturating_sub(4));
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, rect);
    frame.render_widget(
        // No key chip: `framed` spells one `^c`, and Ctrl-C is how this
        // dashboard quits. The key that opens this is in the `?` list, and
        // anyone reading this overlay has already pressed it.
        Paragraph::new(lines).block(framed("MCP CONNECTOR", "", ore::gold())),
        rect,
    );
}

/// Split a line into chunks that fit, on spaces where there are any.
///
/// The command has spaces in it and the token does not, so this has to do
/// both: break where it can, and cut where it cannot rather than running off
/// the side of the overlay.
fn wrapped(text: &str, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split(' ') {
        let mut word = word;
        // A single word longer than the box: cut it into pieces that fit.
        while word.chars().count() > width {
            let cut = word
                .char_indices()
                .nth(width)
                .map_or(word.len(), |(at, _)| at);
            out.push(word[..cut].to_string());
            word = &word[cut..];
        }
        match out.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => out.push(word.to_string()),
        }
    }
    out
}

/// A panel, captioned with the key that shows and hides it, set in a block —
/// the shortcut sits on the thing it acts on rather than only in the help
/// overlay.
fn framed(title: &str, key: &str, color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        // One step up from the dashboard ground, so panels read as raised.
        .style(Style::default().bg(theme::bg()))
        .border_style(Style::default().fg(ore::netherite()))
        .title(Line::from(vec![
            Span::styled(
                if key.is_empty() {
                    " ".to_string()
                } else {
                    format!(" \u{25a0}^{key} ")
                },
                Style::default()
                    .fg(ore::gold())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{title} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ]))
}

/// Width the spinner needs when a fetch is in flight: two spaces and a glyph.
const SPINNER_COLUMN: usize = 3;
/// Width the avatar needs at the far end of the toolbar: the badge, the space
/// that keeps it off the border, and a gap wide enough that it reads as its own
/// thing rather than as one more realm chip.
const AVATAR_COLUMN: usize = avatar::CELLS as usize + AVATAR_INSET + 2;
/// The badge sits one column in from the right border, mirroring the space the
/// brand is given on the left.
const AVATAR_INSET: usize = 1;

/// Cells the day span leaves in hand for the property name beside it. Enough
/// for a short name and its ellipsis — under that the day span goes instead.
const TITLE_STUB: usize = 8;

/// A short label for a realm chip.
///
/// Truncating to three characters renders "United States" and "United Kingdom"
/// as the same "Uni", so two different realms read identically in the header.
/// Multi-word names collapse to their initials instead — US, UK, UAE — while
/// single-word names keep their first three letters. Lowercase joining words
/// ("and", "of") are skipped so "Bosnia and Herzegovina" is BH, not BAH.
fn realm_abbrev(country: &str) -> String {
    let initials: String = country
        .split_whitespace()
        .filter(|word| word.chars().next().is_some_and(char::is_uppercase))
        .filter_map(|word| word.chars().next())
        .collect();

    if initials.chars().count() > 1 {
        initials
    } else {
        country.chars().take(3).collect()
    }
}

/// The country chips for the header, ordered by headcount and capped at five.
///
/// A chip is emitted only if it fits whole. Letting the terminal clip instead
/// leaves a half-written country against the border — "Bra:8" arriving as "B"
/// reads as a rendering fault rather than a boundary.
fn realm_chips(realms: &[(String, f64)], budget: usize) -> Vec<(String, String)> {
    let mut sorted: Vec<&(String, f64)> = realms.iter().collect();
    sorted.sort_by(|a, b| b.1.total_cmp(&a.1));

    let mut chips: Vec<(String, String)> = Vec::new();
    let mut used = 0usize;
    for (country, count) in sorted.iter().take(5) {
        let sep = if chips.is_empty() { " · " } else { "  " };
        let chip = format!("{}:{}", realm_abbrev(country), *count as u64);
        // Count columns, not bytes — a realm name is not necessarily ASCII.
        let width = sep.chars().count() + chip.chars().count();
        if used + width > budget {
            break;
        }
        used += width;
        chips.push((sep.to_string(), chip));
    }
    chips
}

fn header(dash: &Dash, width: u16) -> Paragraph<'static> {
    let phase = dash.phase();
    // A slow sine breathes the live dot; the glyph steps through three sizes so
    // it still reads as a pulse on a terminal without truecolor.
    let breath = (phase * 2.2).sin() * 0.5 + 0.5;
    let dot = glyph::PULSE[((breath * 2.99) as usize).min(2)];

    // The bar gives its pieces up in tiers rather than running off the edge.
    // A narrow terminal used to draw every span whatever the room and let the
    // border crop the last one, which is how a 60-column window ended up
    // saying "◉ 130 on". What goes first is the day span — the header below
    // repeats the window anyway — then the property name is truncated. The
    // brand, the live dot and the count it belongs to are never cut.
    let brand = format!(" {} ANACRAFT ", glyph::PICKAXE);
    let star = if dash.supporter {
        format!("{} ", glyph::STAR)
    } else {
        String::new()
    };
    let pulse = format!("· {dot} ");
    let online = format!("{} online now", commas(dash.live.shown.round()));
    // One cell short of the border, not flush against it: a line that ends on
    // the last cell inside the frame reads as a line that ran out of room,
    // whether or not anything was actually lost.
    let room = width.saturating_sub(3) as usize;
    let fixed = brand.chars().count()
        + star.chars().count()
        + pulse.chars().count()
        + online.chars().count();

    // The day span keeps a few cells in hand for a name beside it: a bar that
    // spends its last cells saying "last 7 days" and then has nothing left to
    // name the property with has kept the wrong half.
    let days = format!("· last {} days ", dash.days);
    let days = if fixed + days.chars().count() + TITLE_STUB <= room {
        days
    } else {
        String::new()
    };
    // Truncated with its trailing space added after, not before: cutting the
    // formatted string takes the space with it and the ellipsis ends up butted
    // against the next span — "CONTOSO LAB…· last 7 days".
    let title = format!(
        "{} ",
        truncate(
            &dash.title.to_uppercase(),
            room.saturating_sub(fixed + days.chars().count() + 1),
        )
    );

    let mut spans = vec![
        Span::styled(
            brand,
            Style::default()
                .fg(ore::grass())
                .add_modifier(Modifier::BOLD),
        ),
        // A subscriber's star, in gold and only when earned. It rides beside the
        // brand rather than out at the end of the line, where the realm chips
        // spend whatever room is left and would eventually push it off screen.
        Span::styled(
            star,
            Style::default()
                .fg(ore::gold())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            title,
            Style::default()
                .fg(ore::diamond())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(days, Style::default().fg(ore::stone())),
        Span::styled(
            pulse,
            Style::default().fg(theme::mix(theme::accent_deep(), theme::bright(), breath)),
        ),
        Span::styled(
            online,
            Style::default().fg(ore::xp()).add_modifier(Modifier::BOLD),
        ),
    ];

    // Top realms with their counts — where the players are right now. They get
    // whatever room is left inside the border, minus the space the spinner will
    // want if a fetch is in flight.
    let spinning = dash.in_flight > 0 || dash.live_fetching;
    let spent: usize = spans.iter().map(|span| span.width()).sum();
    let budget = (width as usize)
        .saturating_sub(2) // the block's own borders
        .saturating_sub(spent)
        .saturating_sub(if spinning { SPINNER_COLUMN } else { 0 })
        .saturating_sub(AVATAR_COLUMN);

    for (i, (sep, chip)) in realm_chips(&dash.live_realms, budget)
        .into_iter()
        .enumerate()
    {
        spans.push(Span::styled(sep, Style::default().fg(ore::netherite())));
        spans.push(Span::styled(
            chip,
            Style::default()
                .fg(theme::ramp(i))
                .add_modifier(Modifier::BOLD),
        ));
    }

    if spinning {
        spans.push(Span::styled(
            format!("  {}", spinner(phase)),
            Style::default().fg(theme::accent_deep()),
        ));
    }

    // The avatar rides the far end of the bar. Everything before it is
    // left-aligned and the chips take a variable amount of room, so the gap is
    // whatever is left over — which also keeps the badge still while the chips
    // behind it change width.
    let used: usize = spans.iter().map(|span| span.width()).sum();
    let inner = width.saturating_sub(2) as usize;
    let badge = avatar::CELLS as usize + AVATAR_INSET;
    // On a narrow bar there is no gap left to hold it away from the text, and
    // a badge butted against the count is worse than no badge: it reads as
    // part of the number. Below that it steps off the bar entirely.
    if used + badge < inner {
        spans.push(Span::raw(" ".repeat(inner - used - badge)));
        spans.push(Span::styled(
            dash.avatar.glyphs(),
            Style::default()
                .fg(dash.avatar.color())
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" ".repeat(AVATAR_INSET)));
    }

    Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ore::netherite()))
            .style(Style::default().bg(theme::bg_lift())),
    )
}

/// The vitals, drawn to fit the rows the layout handed over.
///
/// Three densities rather than one: the panel with its gaps, the same panel
/// closed up, and — where the chart and the map have taken what they need — a
/// line of numbers per metric with the bars given up. Every density says the
/// same six numbers; what goes first is the space around them, then the shape
/// of them, and never the number itself.
fn metrics_panel(dash: &Dash, width: u16, height: u16) -> Paragraph<'static> {
    let bars = height >= VITALS_TIGHT_ROWS;
    let gaps = height >= VITALS_ROWS;
    let phase = dash.phase();

    // Each metric is one row of `name (field)  value  delta`. The subtitle is
    // the first column to give way on a narrow tile: it is the reassurance,
    // and the number it sits beside is the point. The bar is sized to the
    // panel, so it never runs under the border of a short tile.
    //
    // The subtitle column is measured rather than fixed: craft's widest is
    // `avg. session`, boring mode's is `averageSessionDuration`.
    let inner = width.saturating_sub(2) as usize;
    let sub_column = OVERVIEW
        .iter()
        .map(|metric| metric.sub().chars().count() + 2)
        .max()
        .unwrap_or(NAME_COLUMN);
    // Shed whole, never cut: `averageSessionDur…` is not a field anybody can
    // type into GA4.
    let subtitle = inner > NAME_COLUMN + sub_column + VALUE_COLUMN + DELTA_COLUMN;
    let bar_cells = inner.saturating_sub(2).min(30);
    let mut lines: Vec<Line> = Vec::new();

    for (i, metric) in OVERVIEW.iter().enumerate() {
        let Some(row) = dash.metrics.get(i) else {
            continue;
        };
        let flash = flash_level(row.flash);
        // A row that just changed brightens and decays back to its own color,
        // so a refresh is visible even when the number barely moves.
        let label = theme::brighten((metric.color)(), flash * 0.7);

        let mut spans = vec![Span::styled(
            format!("{:<NAME_COLUMN$}", metric.label()),
            Style::default().fg(label).add_modifier(Modifier::BOLD),
        )];
        if subtitle {
            spans.push(Span::styled(
                format!("{:<sub_column$}", format!("({})", metric.sub())),
                Style::default().fg(ore::stone()),
            ));
        }
        spans.push(Span::styled(
            format!("{:>10}  ", value(metric, row.value.shown)),
            Style::default()
                .fg(theme::mix(theme::fg(), theme::bright(), flash))
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(delta_span(
            row.value.target,
            row.previous,
            metric.api == "bounceRate",
        ));
        lines.push(Line::from(spans));

        if bars {
            lines.push(Line::from(bar_spans(
                row.frac.shown,
                bar_cells,
                metric.bar_glyph(),
                (metric.color)(),
                phase,
                row.frac.moving(),
                2,
            )));
        }
        if gaps {
            lines.push(Line::from(""));
        }
    }

    Paragraph::new(lines).block(framed(theme::say("VITALS", "OVERVIEW"), "5", ore::grass()))
}

/// Rank badges: the top three chunks are ore, the rest are plain stone. It is
/// a leaderboard, so it may as well look like one.
fn tier(rank: usize) -> (char, Color) {
    match rank {
        0 => ('\u{25c6}', ore::diamond()),
        1 => ('\u{25c8}', ore::gold()),
        2 => ('\u{25c7}', ore::iron()),
        _ => ('\u{00b7}', ore::stone()),
    }
}

/// How far a chunk climbed or fell since the last report.
fn moved_span(moved: Option<i64>) -> Span<'static> {
    match moved {
        None => Span::styled(
            format!("{:>width$}", "NEW", width = MOVED_COLUMN),
            Style::default()
                .fg(ore::gold())
                .add_modifier(Modifier::BOLD),
        ),
        Some(0) | Some(i64::MIN..=-100) => Span::raw(" ".repeat(MOVED_COLUMN)),
        Some(places) => Span::styled(
            format!(
                "{:>width$}",
                format!(
                    "{}{}",
                    if places > 0 { glyph::UP } else { glyph::DOWN },
                    places.abs()
                ),
                width = MOVED_COLUMN
            ),
            Style::default().fg(if places > 0 {
                ore::emerald()
            } else {
                ore::redstone()
            }),
        ),
    }
}

/// Daily villagers, as bars rather than a seven-character sparkline: the panel
/// is wide, so the days may as well be readable.
fn trend_panel(dash: &Dash, width: u16) -> Paragraph<'static> {
    let inner = width.saturating_sub(2) as usize;
    let mut lines = if dash.daily.len() > 1 {
        daily_chart(&dash.daily, inner, 3)
    } else {
        vec![Line::from(Span::styled(
            "  not enough days yet",
            Style::default().fg(ore::stone()),
        ))]
    };

    if let Some(caption) = trend_caption(&dash.daily, inner) {
        lines.push(Line::from(Span::styled(
            caption,
            Style::default().fg(theme::fade(theme::sage(), 0.2)),
        )));
    }

    Paragraph::new(lines).block(framed("DAILY USERS", "7", ore::grass()))
}

/// The line under the bars: how many days are drawn, and the tallest of them.
/// There is nothing to say before the first day's numbers arrive.
fn trend_caption(daily: &[f64], inner: usize) -> Option<String> {
    if daily.is_empty() {
        return None;
    }

    let shown = visible_days(inner, daily.len());
    let peak = daily[daily.len() - shown..]
        .iter()
        .cloned()
        .fold(0.0_f64, f64::max);
    Some(if shown < daily.len() {
        format!(
            "  last {} of {} days · peak {}",
            shown,
            daily.len(),
            commas(peak)
        )
    } else {
        format!("  {shown} days · peak {}", commas(peak))
    })
}

/// How many days a panel this wide can draw, at one column per day minimum.
fn visible_days(width: usize, total: usize) -> usize {
    total.min(width.saturating_sub(2).max(1)).max(1)
}

/// A column chart `rows` tall. Each day gets as many columns as the width
/// allows, and the eighth-height blocks give each bar eight times the vertical
/// resolution of the row it ends in.
fn daily_chart(values: &[f64], width: usize, rows: usize) -> Vec<Line<'static>> {
    // A narrow panel can't hold a bar per day; it shows the most recent ones
    // rather than squeezing every day into nothing.
    let days = visible_days(width, values.len());
    let values = &values[values.len() - days..];
    let peak = values.iter().cloned().fold(f64::MIN, f64::max).max(1.0);

    // Bars are measured from zero: for a count, a truncated axis would turn a
    // quiet week into a dramatic one.
    //
    // The width has to hold `days` bars, the gaps between them and the indent,
    // or the last bar runs under the border.
    let available = width.saturating_sub(2);
    let gap = usize::from(available >= days * 3);
    let bar = (available.saturating_sub(gap * (days - 1)) / days).clamp(1, 8);

    (0..rows)
        .map(|row| {
            // Row 0 is the top of the chart.
            let floor = (rows - row - 1) as f64;
            let mut spans = vec![Span::raw("  ")];
            for (i, value) in values.iter().enumerate() {
                let height = value / peak * rows as f64 - floor;
                let glyph = if height >= 1.0 {
                    glyph::FULL
                } else if height <= 0.0 {
                    ' '
                } else {
                    glyph::SPARK[((height * 8.0).ceil() as usize).clamp(1, 8) - 1]
                };
                // The latest day is the one people look for, so it is lit.
                let color = if i + 1 == values.len() {
                    theme::bright()
                } else {
                    theme::mix(
                        theme::accent_deep(),
                        ore::grass(),
                        i as f64 / values.len() as f64,
                    )
                };
                spans.push(Span::styled(
                    glyph.to_string().repeat(bar),
                    Style::default().fg(color),
                ));
                if i + 1 < values.len() {
                    spans.push(Span::raw(" ".repeat(gap)));
                }
            }
            Line::from(spans)
        })
        .collect()
}

/// A very rough world, one `#` per land cell: 60 columns of longitude by 12
/// rows spanning 78°N to -57°S, which is where the populated land is. The
/// blank polar rows are left out so a short panel doesn't spend half its
/// height on empty ocean.
const WORLD: [&str; 12] = [
    "    ################   ####      ########################## ",
    "   #################   ###   ############################## ",
    "    ################        ######## ###################### ",
    "     ##############         ####### ######################  ",
    "       ##########          #############################    ",
    "            #####         ############     ###  ######      ",
    "                 #####     ###########           #####      ",
    "                 #######    #########            #######    ",
    "                 #######     #######              #######   ",
    "                  #####       #####               ######    ",
    "                   ###                                  ##  ",
    "                   ##                                       ",
];
/// Latitudes the template's first and last rows sit at.
const WORLD_TOP: f64 = 78.0;
const WORLD_BOTTOM: f64 = -57.0;

/// Approximate centroids for the countries GA reports most often. A country
/// missing from here still shows in the panel's caption — it just doesn't get
/// a dot.
const PLACES: [(&str, f64, f64); 46] = [
    ("United States", 39.0, -98.0),
    ("Canada", 56.0, -106.0),
    ("Mexico", 23.0, -102.0),
    ("Brazil", -10.0, -55.0),
    ("Argentina", -34.0, -64.0),
    ("Chile", -33.0, -71.0),
    ("Colombia", 4.0, -73.0),
    ("Peru", -10.0, -76.0),
    ("United Kingdom", 54.0, -2.0),
    ("Ireland", 53.0, -8.0),
    ("France", 46.0, 2.0),
    ("Spain", 40.0, -4.0),
    ("Portugal", 39.0, -8.0),
    ("Germany", 51.0, 10.0),
    ("Netherlands", 52.0, 5.0),
    ("Belgium", 51.0, 4.0),
    ("Switzerland", 47.0, 8.0),
    ("Austria", 47.0, 14.0),
    ("Italy", 42.0, 12.0),
    ("Poland", 52.0, 19.0),
    ("Czechia", 50.0, 15.0),
    ("Sweden", 62.0, 15.0),
    ("Norway", 61.0, 8.0),
    ("Denmark", 56.0, 10.0),
    ("Finland", 64.0, 26.0),
    ("Ukraine", 49.0, 32.0),
    ("Romania", 46.0, 25.0),
    ("Greece", 39.0, 22.0),
    ("Turkey", 39.0, 35.0),
    ("Russia", 60.0, 90.0),
    ("Israel", 31.0, 35.0),
    ("United Arab Emirates", 24.0, 54.0),
    ("Saudi Arabia", 24.0, 45.0),
    ("Egypt", 27.0, 30.0),
    ("Nigeria", 10.0, 8.0),
    ("Kenya", 0.0, 38.0),
    ("South Africa", -29.0, 24.0),
    ("India", 21.0, 78.0),
    ("Pakistan", 30.0, 70.0),
    ("Bangladesh", 24.0, 90.0),
    ("China", 35.0, 105.0),
    ("Japan", 36.0, 138.0),
    ("South Korea", 36.0, 128.0),
    ("Singapore", 1.0, 104.0),
    ("Indonesia", -2.0, 118.0),
    ("Australia", -25.0, 134.0),
];

/// Land nobody arrived from — the map's ground.
///
/// One quiet shade, not two. Mixing `█` and `▓` per cell was meant to read as
/// placed blocks, but in a single color they differ only in density, so the map
/// came out as dithered static that buried the realms lit on top of it. A light
/// shade gives the continents their silhouette back and leaves the full block
/// free to mean "somebody arrived from here".
const LAND: &str = "\u{2591}";

/// Where a country lands on a `cols` x `rows` grid, if we know it. The grid
/// covers the template's latitude band rather than the whole globe.
fn place(country: &str, cols: usize, rows: usize) -> Option<(usize, usize)> {
    let (_, lat, lon) = PLACES.iter().find(|(name, _, _)| *name == country)?;
    let span = WORLD_TOP - WORLD_BOTTOM;
    let x = ((lon + 180.0) / 360.0 * cols as f64) as usize;
    let y = ((WORLD_TOP - lat) / span * rows as f64).max(0.0) as usize;
    Some((x.min(cols - 1), y.min(rows - 1)))
}

/// Nudges a dot onto the nearest land cell. Centroids are approximate and the
/// map is coarse, so without this a country can end up a cell out to sea.
fn snap(land: &[Vec<bool>], x: usize, y: usize) -> (usize, usize) {
    if land[y][x] {
        return (x, y);
    }
    let mut best = None;
    for (dy, row) in land.iter().enumerate() {
        for (dx, is_land) in row.iter().enumerate() {
            if !is_land {
                continue;
            }
            let distance = (dx as i64 - x as i64).pow(2) + 2 * (dy as i64 - y as i64).pow(2);
            if distance <= 8 && best.map(|(d, _, _)| distance < d).unwrap_or(true) {
                best = Some((distance, dx, dy));
            }
        }
    }
    best.map(|(_, dx, dy)| (dx, dy)).unwrap_or((x, y))
}

/// Who is online, on a map. Land is drawn in shadow and the countries with
/// players are lit on top of it, brightest where the most are.
fn map_panel(dash: &Dash, width: u16, height: u16) -> Paragraph<'static> {
    let cols = (width.saturating_sub(4) as usize).clamp(20, WORLD[0].len());
    let rows = (height.saturating_sub(3) as usize).clamp(4, WORLD.len());
    let phase = dash.phase();

    // Sample the template down to the panel's size. A cell is land if any
    // template cell it covers is land, so shrinking thins the coasts rather
    // than punching holes in them.
    let land: Vec<Vec<bool>> = (0..rows)
        .map(|y| {
            let from = y * WORLD.len() / rows;
            let to = ((y + 1) * WORLD.len() / rows).max(from + 1);
            (0..cols)
                .map(|x| {
                    let left = x * WORLD[0].len() / cols;
                    let right = ((x + 1) * WORLD[0].len() / cols).max(left + 1);
                    WORLD[from..to]
                        .iter()
                        .any(|row| row.as_bytes()[left..right.min(row.len())].contains(&b'#'))
                })
                .collect()
        })
        .collect();

    let mut grid: Vec<Vec<Option<Color>>> = land
        .iter()
        .map(|row| {
            row.iter()
                .map(|is_land| is_land.then(theme::shadow))
                .collect()
        })
        .collect();

    // Base layer: where the period's users came from, weighted by how many.
    let peak = dash
        .realms
        .iter()
        .map(|(_, users)| *users)
        .fold(1.0_f64, f64::max);

    for (country, users) in &dash.realms {
        if *users <= 0.0 {
            continue;
        }
        let Some((x, y)) = place(country, cols, rows) else {
            continue;
        };
        let (x, y) = snap(&land, x, y);
        // Square root, or one dominant country flattens every other realm to
        // the dimmest shade on the map.
        let weight = (users / peak).clamp(0.0, 1.0).sqrt();
        grid[y][x] = Some(theme::mix(theme::accent_deep(), ore::grass(), weight));
    }

    // Overlay: countries with somebody on the site this minute, breathing on
    // the same clock as the rest of the dashboard.
    let breath = (phase * 2.2).sin() * 0.5 + 0.5;
    for (country, users) in &dash.live_realms {
        if *users <= 0.0 {
            continue;
        }
        let Some((x, y)) = place(country, cols, rows) else {
            continue;
        };
        let (x, y) = snap(&land, x, y);
        grid[y][x] = Some(theme::mix(theme::accent(), theme::bright(), breath));
    }

    let mut lines: Vec<Line> = grid
        .into_iter()
        .map(|row| {
            let mut spans = vec![Span::raw("  ")];
            for cell in row {
                spans.push(match cell {
                    // Open water.
                    None => Span::raw(" "),
                    // Land nobody arrived from — the ground the realms sit on.
                    Some(color) if color == theme::shadow() => {
                        Span::styled(LAND, Style::default().fg(color))
                    }
                    // A realm with traffic reads as an ore seam in that ground:
                    // the full block, lit by the ore's own color.
                    Some(color) => Span::styled(
                        glyph::FULL.to_string(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                });
            }
            Line::from(spans)
        })
        .collect();

    // The countries themselves, since a dot on a map this rough is not a label.
    // As many as the width holds, rather than a fixed three and an ellipsis.
    let mut top: Vec<&(String, f64)> = dash.realms.iter().collect();
    top.sort_by(|a, b| b.1.total_cmp(&a.1));

    let online = dash
        .live_realms
        .iter()
        .filter(|(_, users)| *users > 0.0)
        .count();
    // The tally of realms with someone in them rides at the end, but only
    // while a name can still sit in front of it: on a narrow tile the names
    // are the caption's job and the tally is what it trims.
    let suffix = format!(" · {online} online now");
    let inner = (width as usize).saturating_sub(4);
    let suffix = if suffix.chars().count() + CAPTION_NAME <= inner {
        suffix
    } else {
        String::new()
    };
    let budget = inner.saturating_sub(suffix.chars().count());

    let mut named = String::new();
    for (name, users) in top.iter().take(4) {
        let piece = format!("{name} {}", commas(*users));
        let candidate = if named.is_empty() {
            piece
        } else {
            format!("{named} · {piece}")
        };
        if candidate.chars().count() > budget {
            break;
        }
        named = candidate;
    }
    if named.is_empty() {
        named = match top.first() {
            // Nothing fit whole. The leading realm goes in cut rather than the
            // caption telling a narrow tile the map is empty when it is not —
            // which is what a 74-column terminal was reading.
            Some((name, users)) => truncate(&format!("{name} {}", commas(*users)), budget),
            None => truncate(
                theme::say("no realms in this window", "no countries in this window"),
                budget,
            ),
        };
    }

    lines.push(Line::from(Span::styled(
        format!("  {named}{suffix}"),
        Style::default().fg(theme::fade(theme::sage(), 0.2)),
    )));

    Paragraph::new(lines).block(framed("COUNTRIES", "3", ore::lapis()))
}

/// What sits under the vitals in the left-hand column.
enum Stack {
    Map,
    Events,
    Vitals,
}

/// Which panel occupies a slot in the right-hand column.
enum Column {
    Live,
    Chunks,
    RealmsRanked,
    Portals,
    Trend,
}

/// Events per day, this period drawn over the last one.
///
/// A line chart rather than the ranked bars this replaced: the question the
/// panel answers is "are events climbing or falling", which a per-event
/// leaderboard cannot show at all — it ranks names, and the ranking barely
/// moves. Both periods share one y scale, and the current one is drawn second so
/// it sits on top where they cross.
/// One line of the events chart.
///
/// A function rather than a closure inside the panel: the `Dataset` borrows the
/// points it was handed, and that is a lifetime a closure cannot name.
fn events_line(color: Color, points: &[(f64, f64)]) -> Dataset<'_> {
    Dataset::default()
        .marker(symbols::Marker::HalfBlock)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(color))
        .data(points)
}

/// The events legend, and how many name lines it managed to name.
///
/// It rides the bottom border rather than sitting inside the plot: ratatui
/// hides its own legend once a panel is short, and the left column never gives
/// this one the rows it would want.
///
/// Which is also what limits the breakdown. The border is one row, so the
/// legend is one row, so the number of lines the chart can honestly draw is
/// however many fit along it — every one on a wide terminal, the top one or two
/// at eighty columns, and none at all on a panel narrow enough that the two
/// periods are all it can say. The chart is read left to right; a colour nobody
/// can look up is noise in it.
fn events_legend(dash: &Dash, width: u16) -> (Line<'static>, usize) {
    let swatch = |color: Color| Span::styled("\u{2501}\u{2501} ", Style::default().fg(color));
    let label = |text: String| Span::styled(text, Style::default().fg(theme::sage()));

    let mut spans = vec![
        swatch(theme::accent()),
        label(format!("last {} days  ", dash.days)),
        swatch(theme::accent_deep()),
        label("previous  ".to_string()),
    ];
    // What the comparison already spends, plus the space the border keeps at
    // each end of a title.
    let mut spent: usize = spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum::<usize>()
        + 2;

    let mut named = 0;
    for ((name, _), ore) in dash.events.names.iter().zip(event_ores()) {
        // Swatch, name, and the two spaces that keep it off the next entry.
        let entry = 3 + name.chars().count() + 2;
        if spent + entry > width as usize {
            break;
        }
        spans.push(swatch(ore));
        spans.push(label(format!("{name}  ")));
        spent += entry;
        named += 1;
    }

    (Line::from(spans).right_aligned(), named)
}

fn events_panel(dash: &Dash, width: u16) -> Chart<'_> {
    let trend = &dash.events;
    // An empty or flat series would collapse the y axis onto a single row.
    let peak = trend.peak.max(1.0);
    let last = trend.current.len().saturating_sub(1).max(1) as f64;

    // The legend decides how many name lines are drawn, not the other way
    // round: a line nothing names is a mystery rather than information, so the
    // chart shows every one the panel is wide enough to label and no more.
    let (legend, named) = events_legend(dash, width);

    // Names first, then the two period lines over the top of them. Every name
    // is part of the total, so where they touch it is the total that should
    // survive the overdraw — the alternative is a headline line with holes
    // punched in it by its own components.
    let mut datasets: Vec<Dataset> = trend
        .names
        .iter()
        .take(named)
        .zip(event_ores())
        .map(|((_, points), ore)| events_line(ore, points))
        .collect();
    datasets.push(events_line(theme::accent_deep(), &trend.previous));
    datasets.push(events_line(theme::accent(), &trend.current));

    // The headline rides on the border, where the panel has room for it.
    let headline = Line::from(vec![
        Span::styled(
            format!(" {} ", commas(trend.total)),
            Style::default()
                .fg(theme::bright())
                .add_modifier(Modifier::BOLD),
        ),
        delta_span(trend.total, trend.total_previous, false),
        Span::raw(" "),
    ])
    .right_aligned();

    Chart::new(datasets)
        .style(Style::default().bg(theme::bg()))
        .block(
            framed("EVENTS", "1", ore::xp())
                .title_top(headline)
                .title_bottom(legend),
        )
        .legend_position(None)
        .x_axis(
            Axis::default()
                .style(Style::default().fg(theme::shadow()))
                .bounds([0.0, last])
                .labels(axis_days(&trend.days)),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(theme::shadow()))
                .bounds([0.0, peak])
                .labels(axis_counts(peak)),
        )
}

/// First, middle and last day of the period. Every day would not fit, and
/// ratatui spreads whatever it is given evenly across the axis.
fn axis_days(days: &[String]) -> Vec<Line<'static>> {
    let label = |text: &str| {
        Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(theme::sage()),
        ))
    };
    match days.len() {
        0 => Vec::new(),
        1 => vec![label(&days[0])],
        n => vec![label(&days[0]), label(&days[n / 2]), label(&days[n - 1])],
    }
}

/// Zero, half and full scale up the y axis.
fn axis_counts(peak: f64) -> Vec<Line<'static>> {
    [0.0, peak / 2.0, peak]
        .iter()
        .map(|value| {
            Line::from(Span::styled(
                commas(value.round()),
                Style::default().fg(theme::sage()),
            ))
        })
        .collect()
}

/// The realtime panel: the count, an htop-style meter, the scrolling trace,
/// and the arrivals behind the last few changes.
/// The realtime graph: one column per poll, newest at the right.
///
/// This is the panel the product is known for, and the site's hero has been
/// drawing it — animated, in this exact ramp — while the dashboard itself drew
/// no graph at all. `dash.history` had been collecting the samples for one
/// since the field was added; nothing read them. The hero was promising a
/// chart the binary did not have.
///
/// Ported from that hero rather than reinvented, down to the cell rules: body
/// dim with only each column's cap lit, because a chart this dense drawn solid
/// and evenly coloured stops being a chart and becomes a wall with a ragged
/// top. The newest column is lit whole — it is the one arriving.
fn live_graph(history: &VecDeque<f64>, width: u16, rows: usize) -> Vec<Line<'static>> {
    let columns = (width as usize).saturating_sub(5);
    if columns == 0 || rows == 0 {
        return Vec::new();
    }

    // The newest samples, oldest first. Fewer than fit means the chart fills in
    // from the right as polls arrive, rather than stretching a short history
    // across the whole panel and implying samples nobody took.
    let recent: Vec<f64> = history.iter().rev().take(columns).rev().copied().collect();
    let pad = columns - recent.len();

    // Bars from zero, scaled so the busiest poll in view reaches the top. A
    // flat stretch therefore draws as a solid block with one lit line across
    // it, which is what flat looks like.
    let peak = recent.iter().copied().fold(0.0_f64, f64::max);
    if peak <= 0.0 {
        return Vec::new();
    }

    let body = theme::fade(theme::accent_deep(), 0.45);
    let cap = theme::accent();
    let newest = ore::gold();

    let mut lines = Vec::with_capacity(rows);
    for r in 0..rows {
        // Row 0 is the top, so the height a column must reach to put anything
        // here counts down from the top.
        let from_bottom = rows - r;
        let mut cells: Vec<(char, Option<Color>)> = Vec::with_capacity(columns);
        for _ in 0..pad {
            cells.push((' ', None));
        }
        for (c, value) in recent.iter().enumerate() {
            let filled = (value / peak).clamp(0.0, 1.0) * rows as f64;
            let full = filled.floor() as usize;
            let rest = filled - full as f64;

            let (ch, mut color) = if from_bottom <= full {
                (
                    glyph::FULL,
                    Some(if from_bottom == full { cap } else { body }),
                )
            } else if from_bottom == full + 1 && rest > 0.12 {
                // The top of a column lands between two rows, and the ramp is
                // what gets it to the right height instead of rounding there.
                let step = ((rest * 8.0) as usize).min(glyph::SPARK.len() - 1);
                (glyph::SPARK[step], Some(cap))
            } else {
                (' ', None)
            };
            if color.is_some() && c + 1 == recent.len() {
                color = Some(newest);
            }
            cells.push((ch, color));
        }

        // One span per run of like-coloured cells, not one per cell.
        let mut spans = vec![Span::raw("  ")];
        let mut run = String::new();
        let mut current: Option<Color> = cells.first().and_then(|(_, color)| *color);
        for (ch, color) in cells {
            if color != current {
                if !run.is_empty() {
                    spans.push(styled_run(&run, current));
                }
                run.clear();
                current = color;
            }
            run.push(ch);
        }
        if !run.is_empty() {
            spans.push(styled_run(&run, current));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// A run of graph cells sharing one colour. Uncoloured runs are the empty
/// space above a column and carry no style at all.
fn styled_run(run: &str, color: Option<Color>) -> Span<'static> {
    match color {
        Some(color) => Span::styled(run.to_string(), Style::default().fg(color)),
        None => Span::raw(run.to_string()),
    }
}

fn live_panel(dash: &Dash, width: u16) -> Paragraph<'static> {
    let phase = dash.phase();
    let breath = (phase * 2.2).sin() * 0.5 + 0.5;

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("  {} ", glyph::PULSE[((breath * 2.99) as usize).min(2)]),
                Style::default().fg(theme::mix(theme::accent_deep(), theme::bright(), breath)),
            ),
            Span::styled(
                commas(dash.live.shown.round()),
                Style::default()
                    .fg(theme::mix(theme::accent(), theme::bright(), breath))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                theme::say("  players online", "  active users"),
                Style::default().fg(ore::stone()),
            ),
        ]),
        Line::from(Span::styled(
            truncate(
                &format!(
                    "  active last 30 min · polled {}s",
                    dash.live_every.as_secs(),
                ),
                width.saturating_sub(3) as usize,
            ),
            Style::default().fg(theme::fade(theme::sage(), 0.3)),
        )),
    ];

    // The graph goes in the three rows `LIVE_ROWS` already budgeted and the
    // content never used, so nothing else on the column gives up height for
    // it. It also does the separating that the blank line used to.
    lines.extend(live_graph(&dash.history, width, LIVE_GRAPH_ROWS));
    lines.push(Line::from(""));

    // Event feed — recent arrivals and departures.
    //
    // Always FEED_ROWS tall. Anything not lit is drawn as unlit cells, so the
    // field is the same size whether six things just happened or nothing has.
    let mut feed_rows = 0usize;
    if dash.feed.is_empty() {
        lines.push(Line::from(Span::styled(
            "  quiet out there",
            Style::default().fg(theme::fade(theme::sage(), 0.3)),
        )));
        feed_rows += 1;
    } else {
        // The feed reads as a small LCD: one hue, hierarchy spent entirely on
        // brightness, and every row sitting on a field of unlit cells.
        //
        // Monochrome is the point. The old rows coloured rising green and
        // falling purple, which put direction in the one channel an LCD does
        // not have — so direction moves to the glyph, where ▲/▼ carries it even
        // on a terminal with no colour at all. The panel's own hue comes from
        // the palette, so each theme lights its screen its own way.
        let lit = theme::accent();
        for entry in dash.feed.iter().take(6) {
            let age = entry.at.elapsed().as_secs_f64() / FEED_TTL.as_secs_f64();
            let rising = entry.delta > 0.0;
            let time_ago = entry.at.elapsed().as_secs();
            let time_str = if time_ago < 60 {
                format!("{}s", time_ago)
            } else {
                format!("{}m", time_ago / 60)
            };

            let glyph_cell = format!("  {} ", if rising { glyph::UP } else { glyph::DOWN });
            let delta_cell = format!("{:>4} ", format!("{:+}", entry.delta as i64));
            let label_cell = if rising {
                theme::say("spawned in", "arrived")
            } else {
                theme::say("wandered off", "left")
            }
            .to_string();
            let time_cell = format!("  {}", time_str);

            // Unlit cells fill the rest of the row, so the field is visible
            // where nothing is lit — that texture is what separates a dot
            // matrix from a plain list. Ages out with the row it belongs to.
            let used = glyph_cell.chars().count()
                + delta_cell.chars().count()
                + label_cell.chars().count()
                + time_cell.chars().count();
            let unlit: String = std::iter::repeat(glyph::UNLIT)
                .take((width as usize).saturating_sub(used + 3))
                .collect();

            lines.push(Line::from(vec![
                Span::styled(glyph_cell, Style::default().fg(theme::fade(lit, age))),
                Span::styled(
                    delta_cell,
                    Style::default()
                        .fg(theme::fade(lit, age))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    label_cell,
                    // A dimmer run of the same hue, never a second colour.
                    Style::default().fg(theme::fade(lit, (age + 0.45).min(1.0))),
                ),
                Span::styled(
                    time_cell,
                    Style::default().fg(theme::fade(lit, (age + 0.65).min(1.0))),
                ),
                Span::styled(
                    unlit,
                    Style::default().fg(theme::fade(lit, (age + 0.86).min(1.0))),
                ),
            ]));
            feed_rows += 1;
        }
    }

    // The dark rest of the screen. Dimmer than the faintest live row, so it
    // reads as field rather than as an event that has nearly faded out.
    let dark: String = std::iter::repeat(glyph::UNLIT)
        .take((width as usize).saturating_sub(5))
        .collect();
    for _ in feed_rows..FEED_ROWS {
        lines.push(Line::from(Span::styled(
            format!("  {dark}"),
            Style::default().fg(theme::fade(theme::accent(), 0.93)),
        )));
    }

    Paragraph::new(lines).block(framed("RIGHT NOW", "2", ore::xp()))
}

/// How many two-line rows a list panel can *finish* inside `height`.
///
/// A panel that starts a row it cannot finish leaves a label with nothing
/// under it — no bar, no number — which reads as a rendering fault rather
/// than as a list that ran out of room. The column hands its spare rows out
/// one at a time and these panels spend them two at a time, so an odd row is
/// ordinary rather than exceptional: the last one stays empty on purpose.
fn whole_rows(height: u16) -> usize {
    (height.saturating_sub(2) / 2) as usize
}

fn pages_panel(dash: &Dash, width: u16, height: u16) -> Paragraph<'static> {
    let phase = dash.phase();
    let mut lines: Vec<Line> = Vec::new();

    // Row shape is `   <bar>  <views> <share>`, so the bar gets whatever the
    // indent, the gaps and the two number columns leave — otherwise they run
    // under the border on an 80-column terminal.
    let inner = width.saturating_sub(2) as usize;
    let cells = inner
        .saturating_sub(3 + 2 + VIEWS_COLUMN + SHARE_COLUMN)
        .clamp(4, 20);
    // The label shares its line with the movement marker.
    let label_cells = inner.saturating_sub(4 + MOVED_COLUMN);

    // Share is of *all* page views for the period, not of the eight rows shown,
    // which is why it comes from the headline metric rather than these rows.
    let views_total = OVERVIEW
        .iter()
        .position(|metric| metric.api == "screenPageViews")
        .and_then(|i| dash.metrics.get(i))
        .map(|row| row.value.target)
        .unwrap_or(0.0);

    if dash.pages.is_empty() {
        lines.push(Line::from(Span::styled(
            "no data in this window",
            Style::default().fg(ore::stone()),
        )));
    }

    for (i, row) in dash.pages.iter().take(whole_rows(height)).enumerate() {
        let color = theme::ramp(i);
        let label: String = row.path.chars().take(label_cells).collect();
        let (ore, ore_color) = tier(i);

        let mut heading = vec![
            Span::styled(
                format!("{ore} "),
                Style::default().fg(ore_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{label:<label_cells$}"), Style::default().fg(color)),
        ];
        heading.push(moved_span(row.moved));
        lines.push(Line::from(heading));

        let mut spans = bar_spans(
            row.frac.shown,
            cells,
            glyph::FULL,
            color,
            phase,
            row.frac.moving(),
            3,
        );
        spans.push(Span::styled(
            format!(
                "  {:>width$}",
                commas(row.views.shown.round()),
                width = VIEWS_COLUMN
            ),
            Style::default()
                .fg(theme::fg())
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            if views_total > 0.0 {
                format!(
                    "{:>width$}",
                    format!("{:.0}%", row.views.shown / views_total * 100.0),
                    width = SHARE_COLUMN
                )
            } else {
                " ".repeat(SHARE_COLUMN)
            },
            Style::default().fg(ore::stone()),
        ));
        lines.push(Line::from(spans));
    }

    Paragraph::new(lines).block(framed("TOP PAGES", "4", ore::copper()))
}

fn realms_ranked_panel(dash: &Dash, width: u16, height: u16) -> Paragraph<'static> {
    let phase = dash.phase();
    let inner = width.saturating_sub(2) as usize;
    let cells = inner.saturating_sub(3 + 2 + VIEWS_COLUMN).clamp(4, 20);
    let label_cells = inner.saturating_sub(4 + MOVED_COLUMN);

    let peak = dash.realms.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max);

    let mut sorted: Vec<&(String, f64)> = dash.realms.iter().collect();
    sorted.sort_by(|a, b| b.1.total_cmp(&a.1));

    let mut lines: Vec<Line> = Vec::new();

    if sorted.is_empty() {
        lines.push(Line::from(Span::styled(
            theme::say(
                "no realm data in this window",
                "no country data in this window",
            ),
            Style::default().fg(ore::stone()),
        )));
    }

    for (i, (country, count)) in sorted.iter().take(whole_rows(height).min(8)).enumerate() {
        let color = theme::ramp(i);
        let label: String = country.chars().take(label_cells).collect();
        let (ore_badge, ore_color) = tier(i);
        let frac = if peak > 0.0 { *count / peak } else { 0.0 };

        lines.push(Line::from(vec![
            Span::styled(
                format!("{ore_badge} "),
                Style::default().fg(ore_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{label:<label_cells$}"), Style::default().fg(color)),
            Span::raw(" ".repeat(MOVED_COLUMN)),
        ]));

        let mut spans = bar_spans(frac, cells, glyph::FULL, color, phase, false, 3);
        spans.push(Span::styled(
            format!("  {:>width$}", commas(*count), width = VIEWS_COLUMN),
            Style::default()
                .fg(theme::fg())
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(spans));
    }

    Paragraph::new(lines).block(framed("TOP COUNTRIES", "6", ore::lapis()))
}

/// Who is sending the traffic, ranked by sessions.
///
/// The one panel that answers a question about somebody else. Every other box
/// here reports on the site — what it served, where its readers were, how many
/// were on it — and this one reports on the web around it: who linked, who
/// searched, who mentioned it.
///
/// GA hands back `source / medium` in one string. It is split so the source
/// leads at full strength and the medium trails dim, because the source is the
/// name being looked for and the medium is a footnote about it. `(direct)` is
/// left as GA writes it, parentheses and all — it is not a site, and dressing
/// it up as one would be the panel telling a small lie in its own vocabulary.
fn portals_panel(dash: &Dash, width: u16, height: u16) -> Paragraph<'static> {
    let phase = dash.phase();
    let inner = width.saturating_sub(2) as usize;
    let cells = inner.saturating_sub(3 + 2 + VIEWS_COLUMN).clamp(4, 20);
    let label_cells = inner.saturating_sub(4 + MOVED_COLUMN);

    let peak = dash.portals.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max);

    let mut sorted: Vec<&(String, f64)> = dash.portals.iter().collect();
    sorted.sort_by(|a, b| b.1.total_cmp(&a.1));

    let mut lines: Vec<Line> = Vec::new();

    if sorted.is_empty() {
        lines.push(Line::from(Span::styled(
            theme::say(
                "nobody has sent anyone this way yet",
                "no referral data in this window",
            ),
            Style::default().fg(ore::stone()),
        )));
    }

    for (i, (portal, count)) in sorted.iter().take(whole_rows(height).min(8)).enumerate() {
        let color = theme::ramp(i);
        let (ore_badge, ore_color) = tier(i);
        let frac = if peak > 0.0 { *count / peak } else { 0.0 };

        // `google / organic` -> a lit source and a dim medium. A row with no
        // slash in it is all source, which is what GA does with `(direct)`.
        let (source, medium) = match portal.split_once(" / ") {
            Some((source, medium)) => (source, Some(medium)),
            None => (portal.as_str(), None),
        };
        let source: String = source.chars().take(label_cells).collect();
        // What the source did not use is the medium's, and the two together
        // come to exactly `label_cells` — the width every other panel's label
        // column is drawn at, so the bars below stay in one line down the box.
        let room = label_cells.saturating_sub(source.chars().count());
        let medium: String = match medium {
            // Two of those characters are spoken for: the space that separates
            // the pair, and one that keeps the medium off the bar beside it.
            Some(medium) if room > 3 => {
                let medium: String = medium.chars().take(room - 2).collect();
                format!(" {medium}")
            }
            _ => String::new(),
        };

        lines.push(Line::from(vec![
            Span::styled(
                format!("{ore_badge} "),
                Style::default().fg(ore_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(source, Style::default().fg(color)),
            Span::styled(
                format!("{medium:<room$}"),
                Style::default().fg(ore::stone()),
            ),
            Span::raw(" ".repeat(MOVED_COLUMN)),
        ]));

        let mut spans = bar_spans(frac, cells, glyph::FULL, color, phase, false, 3);
        spans.push(Span::styled(
            format!("  {:>width$}", commas(*count), width = VIEWS_COLUMN),
            Style::default()
                .fg(theme::fg())
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::from(spans));
    }

    Paragraph::new(lines).block(framed(
        theme::say("PORTALS", "TRAFFIC SOURCES"),
        "8",
        ore::emerald(),
    ))
}

fn footer(dash: &Dash, width: u16) -> Paragraph<'static> {
    // Everything the bar can say, less its borders and one cell held back so
    // the last slot never ends flush against the closing one.
    let room = width.saturating_sub(3) as usize;
    if let Some(err) = &dash.error {
        return Paragraph::new(Line::from(Span::styled(
            format!(
                " ⚠ {} (showing last good data)",
                truncate(err, room.saturating_sub(28).max(8))
            ),
            Style::default().fg(ore::redstone()),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ore::redstone()))
                .style(Style::default().bg(theme::bg_lift())),
        );
    }

    // Hotbar-style footer: each key lives in its own slot, separated by
    // netherite walls — like the nine-slot bar at the bottom of the screen.
    let wall = || Span::styled(" │ ", Style::default().fg(ore::netherite()));
    let slot = |k: &str, label: &str| -> Vec<Span<'static>> {
        vec![
            Span::styled(
                format!("[{k}]"),
                Style::default()
                    .fg(ore::gold())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(label.to_string(), Style::default().fg(ore::stone())),
        ]
    };

    let breath = (dash.phase() * 2.2).sin() * 0.5 + 0.5;
    let pulse = if dash.in_flight > 0 {
        Span::styled(
            format!("{}", spinner(dash.phase())),
            Style::default().fg(theme::bright()),
        )
    } else {
        Span::styled(
            format!("{}", glyph::PULSE[((breath * 2.99) as usize).min(2)]),
            Style::default().fg(theme::mix(theme::accent_deep(), theme::bright(), breath)),
        )
    };

    // The bar is built a slot at a time so a narrow terminal can take slots
    // off the end instead of letting the border cut one in half — the last
    // one used to arrive as "· updated" with the clock behind the wall. What
    // goes first is the timestamp, then the live lamp, then the vocabulary and
    // the palette name: the keys are the reason the bar is there and they are
    // what it keeps.
    let mut slots: Vec<Vec<Span<'static>>> = vec![vec![Span::raw(" ")]];

    // Each key is a hotbar slot: [key]label separated by netherite walls.
    for (k, label) in [
        ("q", "quit"),
        ("r", theme::say("rebuild", "refresh")),
        ("?", "help"),
    ] {
        let mut group = vec![wall()];
        group.extend(slot(k, label));
        slots.push(group);
    }
    // Theme slot — the pack name is the item.
    let mut group = vec![wall()];
    group.extend(slot("t", ""));
    group.push(Span::styled(
        theme::palette().name.to_string(),
        Style::default()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD),
    ));
    slots.push(group);

    // Vocabulary slot: the mode is the item, as the pack name is for `t`.
    let mut group = vec![wall()];
    group.extend(slot("b", ""));
    group.push(Span::styled(
        theme::say("craft", "boring").to_string(),
        Style::default()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD),
    ));
    slots.push(group);

    // Live indicator in its own slot.
    slots.push(vec![
        wall(),
        pulse,
        Span::styled(
            " live",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]);

    // Timestamp behind a final wall.
    slots.push(vec![
        wall(),
        Span::styled(
            format!("· updated {}", dash.updated),
            Style::default().fg(ore::stone()),
        ),
    ]);

    // The keys themselves — the leading pad and the three [key] slots — stay
    // whatever the width. Under that there is no bar worth drawing anyway.
    let width_of =
        |group: &Vec<Span<'static>>| group.iter().map(|span| span.width()).sum::<usize>();
    while slots.len() > 4 && slots.iter().map(width_of).sum::<usize>() > room {
        slots.pop();
    }
    let spans: Vec<Span<'static>> = slots.into_iter().flatten().collect();

    Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ore::netherite()))
            .style(Style::default().bg(theme::bg_lift())),
    )
}

// ------------------------------------------------------------- primitives ---

/// A block bar with two pieces of motion: a partial leading cell drawn as one
/// of Minecraft's break stages, and a brighter block sweeping along the mined
/// section, so a bar reads as being dug rather than merely drawn.
///
/// The sweep never stops. While the bar is filling it runs bright and fast, and
/// once the value settles it drops to a slow, barely-there shimmer — the screen
/// is never completely still, which is what makes a dashboard look connected
/// rather than crashed.
fn bar_spans(
    frac: f64,
    cells: usize,
    block: char,
    color: Color,
    phase: f64,
    active: bool,
    indent: usize,
) -> Vec<Span<'static>> {
    let frac = frac.clamp(0.0, 1.0);
    let exact = frac * cells as f64;
    let filled = (exact.floor() as usize).min(cells);
    let remainder = exact - filled as f64;

    // Mute toward the shadow palette — Minecraft's textures are noisy and
    // desaturated, not neon-bright.
    let color = theme::mix(color, theme::shadow(), 0.30);

    let mut spans = vec![Span::raw(" ".repeat(indent))];

    if filled > 0 {
        let (speed, lift) = if active { (12.0, 0.6) } else { (3.0, 0.22) };
        let head = ((phase * speed) as usize) % filled;
        let mut highlight = Style::default().fg(theme::brighten(color, lift));
        if active {
            highlight = highlight.add_modifier(Modifier::BOLD);
        }

        spans.push(Span::styled(
            block.to_string().repeat(head),
            Style::default().fg(color),
        ));
        spans.push(Span::styled(block.to_string(), highlight));
        spans.push(Span::styled(
            block.to_string().repeat(filled - head - 1),
            Style::default().fg(color),
        ));
    }

    let mut used = filled;
    if used < cells && remainder > 0.1 {
        let stage = if remainder > 0.66 {
            glyph::PARTIAL
        } else if remainder > 0.33 {
            glyph::CRACKED
        } else {
            glyph::EMPTY
        };
        spans.push(Span::styled(
            stage.to_string(),
            Style::default().fg(theme::mix(color, theme::shadow(), 0.35)),
        ));
        used += 1;
    }

    if used < cells {
        spans.push(Span::styled(
            glyph::EMPTY.to_string().repeat(cells - used),
            Style::default().fg(theme::shadow()),
        ));
    }

    spans
}

fn spinner(phase: f64) -> char {
    glyph::SPINNER[((phase * 12.0) as usize) % glyph::SPINNER.len()]
}

fn delta_span(current: f64, previous: f64, lower_is_better: bool) -> Span<'static> {
    if previous <= 0.0 {
        return Span::raw("");
    }
    let change = (current - previous) / previous * 100.0;
    if !change.is_finite() || change.abs() < 0.5 {
        return Span::styled("— flat", Style::default().fg(ore::stone()));
    }
    let rising = change > 0.0;
    let good = rising != lower_is_better;
    Span::styled(
        format!(
            "{}{:.0}%",
            if rising { glyph::UP } else { glyph::DOWN },
            change.abs()
        ),
        Style::default().fg(if good {
            ore::emerald()
        } else {
            ore::redstone()
        }),
    )
}

/// Drops spans off the end of a line until what is left fits the room it has,
/// never cutting into the `keep` at the front that carry the point of it.
///
/// A bar that runs past its border does not degrade — the border simply cuts
/// the last span wherever it happens to fall, mid-word and mid-number. Losing
/// a whole trailing span says the same thing honestly.
fn fits(mut spans: Vec<Span<'static>>, room: usize, keep: usize) -> Vec<Span<'static>> {
    while spans.len() > keep && spans.iter().map(|span| span.width()).sum::<usize>() > room {
        spans.pop();
    }
    spans
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars()
        .take(max.saturating_sub(1))
        .chain(['…'])
        .collect()
}

#[cfg(test)]
mod tests {

    /// A config with two properties, the first of them active.
    fn two_properties() -> crate::config::Config {
        let mut cfg = crate::config::Config::default();
        cfg.upsert("552157097", Some("anacraft".into()));
        cfg.upsert("442488241", Some("smartloop.ai".into()));
        cfg.upsert("552157097", None);
        cfg
    }

    #[test]
    fn quitting_on_a_property_makes_it_the_one_the_cli_uses() {
        // The bug: Tab moved the view and nothing else, so somebody who tabbed
        // to another property and quit found `craft overview` and a cron'd
        // `craft watch` still reading the property they had left — while the
        // config's own comment said Tab was how properties are switched.
        let mut cfg = two_properties();
        assert_eq!(cfg.active.as_deref(), Some("552157097"));

        settle(&mut cfg, Some("442488241".into()), "osaka-jade".into());

        assert_eq!(cfg.active.as_deref(), Some("442488241"));
        assert_eq!(cfg.resolve_property(None).unwrap(), "442488241");
    }

    #[test]
    fn the_palette_lands_on_the_property_that_was_on_screen() {
        // The same fault from the other side: pressing `t` after tabbing used
        // to write the palette onto the property no longer being looked at.
        let mut cfg = two_properties();
        settle(&mut cfg, Some("442488241".into()), "catppuccin".into());

        assert_eq!(
            cfg.find("442488241").unwrap().theme.as_deref(),
            Some("catppuccin")
        );
        assert_eq!(cfg.find("552157097").unwrap().theme, None, "wrong property");
    }

    #[test]
    fn a_property_that_is_not_in_the_config_does_not_become_the_default() {
        // Reached with `--property`. One run is not a decision to switch, and
        // silently rewriting `active` from a flag would be a surprise.
        let mut cfg = two_properties();
        settle(&mut cfg, Some("999999".into()), "osaka-jade".into());

        assert_eq!(cfg.active.as_deref(), Some("552157097"));
        assert!(
            cfg.find("999999").is_none(),
            "a flag must not add a property"
        );
    }

    #[test]
    fn the_demo_never_writes_a_property_into_a_real_config() {
        // `run_demo` drives an empty rotation, so it lands nowhere. The theme
        // still persists — it is the one thing the demo is allowed to change.
        let mut cfg = two_properties();
        settle(&mut cfg, None, "gruvbox".into());

        assert_eq!(cfg.active.as_deref(), Some("552157097"));
        assert_eq!(cfg.properties.len(), 2);
        assert_eq!(
            cfg.find("552157097").unwrap().theme.as_deref(),
            Some("gruvbox")
        );
    }

    #[test]
    fn landing_reads_the_rotation_and_the_demo_walks_an_empty_one() {
        let rotation = vec![
            crate::config::Property {
                id: "111".into(),
                ..Default::default()
            },
            crate::config::Property {
                id: "222".into(),
                ..Default::default()
            },
        ];
        assert_eq!(landed(&rotation, 1).as_deref(), Some("222"));
        assert_eq!(landed(&[], 0), None);
    }
    use super::*;

    /// A dashboard on the demo numbers, settled so nothing is mid-ease.
    fn settled_demo() -> Dash {
        let mut synthetic = Synthetic::new();
        let snapshot = synthetic.report(&mut rand::thread_rng());
        let mut dash = Dash::new(
            "test".to_string(),
            7,
            snapshot,
            128.0,
            Vec::new(),
            Duration::from_secs(30),
            Duration::from_secs(5),
        );
        for _ in 0..80 {
            dash.step(FRAME.as_secs_f64());
        }
        dash
    }

    /// A board with every panel switched on.
    ///
    /// The dashboard opens on five of the eight, so a test about how the grid
    /// lays out a *full* board has to ask for one rather than take the
    /// default. These are tests about the grid, not about which panels the
    /// dashboard chooses to open on — that one is pinned on its own, by
    /// `the_board_opens_on_five_tiles`.
    fn crowded_dash() -> Dash {
        let mut dash = capture_dash();
        dash.panels = Panels {
            events: true,
            live: true,
            map: true,
            chunks: true,
            vitals: true,
            realms_ranked: true,
            trend: true,
            portals: true,
        };
        dash
    }

    /// Renders a widget into a fixed grid and hands back the rows as text.
    fn rendered<W: ratatui::widgets::Widget>(width: u16, height: u16, widget: W) -> Vec<String> {
        use ratatui::{backend::TestBackend, Terminal};
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(widget, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_name_that_was_quiet_on_tuesday_still_lines_up_with_tuesday() {
        // The bug this exists to prevent is silent and total: GA sends no row
        // for a day a name did not happen, and a series built from the rows it
        // did send would draw every later point one day to the left. The line
        // stays smooth and stops being about the days underneath it.
        let days = ["20260901", "20260902", "20260903"];
        let aligned = align(
            &days,
            vec![(
                "scroll".to_string(),
                vec![("20260901".to_string(), 9.0), ("20260903".to_string(), 4.0)],
            )],
        );

        assert_eq!(aligned.len(), 1);
        assert_eq!(aligned[0].0, "scroll");
        assert_eq!(aligned[0].1, vec![9.0, 0.0, 4.0], "Tuesday lost its place");
    }

    #[test]
    fn a_name_with_nothing_in_the_period_is_a_flat_line_not_a_short_one() {
        // Every series has to be as long as the axis, or ratatui draws it
        // against the wrong x bounds.
        let days = ["20260901", "20260902"];
        let aligned = align(&days, vec![("purchase".to_string(), Vec::new())]);
        assert_eq!(aligned[0].1, vec![0.0, 0.0]);
    }

    #[test]
    fn the_chart_draws_every_name_the_panel_can_label_and_no_more() {
        // The rule the breakdown lives by: a coloured line nobody can look up
        // is noise, so the legend decides how many are drawn. Narrow panels
        // fall back to the two periods, which is what the chart was before.
        let dash = settled_demo();
        assert!(
            dash.events.names.len() > 1,
            "the demo has no breakdown to draw"
        );

        let (_, narrow) = events_legend(&dash, 40);
        let (_, wide) = events_legend(&dash, 200);
        assert_eq!(
            narrow, 0,
            "a 40-column panel named a line it has no room for"
        );
        assert!(
            wide > narrow,
            "a wide panel named no more than a narrow one"
        );
        assert!(
            wide <= EVENT_NAME_LINES,
            "more lines than there are ores to draw them in"
        );

        // Monotonic, so growing the terminal never takes a line away.
        let mut named = 0;
        for width in 40..=200u16 {
            let (_, count) = events_legend(&dash, width);
            assert!(count >= named, "width {width}: a line was dropped");
            named = count;
        }

        // And the name is on the panel, not just in the count.
        let panel = rendered(200, EVENTS_ROWS, events_panel(&dash, 200)).join("\n");
        assert!(
            panel.contains(&dash.events.names[0].0),
            "the top event name never reached the legend"
        );
    }

    #[test]
    fn the_settled_period_is_only_reused_for_the_question_it_answered() {
        // The cache saves a request per refresh; answering the wrong question
        // with it would draw another property's fortnight under this one.
        *SETTLED.lock().unwrap() = Some(Settled {
            property: "111".to_string(),
            days: 7,
            at: Instant::now(),
            counts: vec![("20260901".to_string(), 12.0)],
        });

        let now = Instant::now();
        assert!(settled_hit("111", 7, now).is_some(), "its own question");
        assert!(settled_hit("222", 7, now).is_none(), "another property");
        assert!(settled_hit("111", 28, now).is_none(), "another window");

        // And it goes quiet once the window it describes could have rolled.
        let later = now.checked_add(SETTLED_FOR).expect("a clock with a future");
        assert!(
            settled_hit("111", 7, later).is_none(),
            "stale and still used"
        );

        *SETTLED.lock().unwrap() = None;
    }

    /// The chart is the panel's whole point, so the two things that make it
    /// readable — a drawn line and the axis it is read against — have to survive
    /// every width the left column can hand it.
    ///
    /// It also has to draw in glyphs the site's font actually carries. The
    /// braille markers this started out with looked best in a terminal, but the
    /// self-hosted JetBrains Mono subset has none of U+2800..U+28FF, so every
    /// plotted cell fell back to a face with a different advance and dragged the
    /// rest of its row out of the grid.
    #[test]
    fn the_events_chart_draws_both_periods_against_an_axis() {
        let dash = settled_demo();
        assert_eq!(dash.events.current.len(), 7);
        assert_eq!(dash.events.previous.len(), 7);

        for width in 40..=120u16 {
            let rows = rendered(width, EVENTS_ROWS, events_panel(&dash, width));
            let panel = rows.join("\n");

            let plotted = panel
                .chars()
                .filter(|c| matches!(c, '\u{2588}' | '\u{2584}' | '\u{2580}'))
                .count();
            assert!(plotted > 20, "width {width}: only {plotted} plotted cells");

            assert!(
                !panel
                    .chars()
                    .any(|c| ('\u{2800}'..='\u{28ff}').contains(&c)),
                "width {width}: braille has no glyph in the site's font"
            );

            // The scale the lines are read against.
            let peak = commas(dash.events.peak.round());
            assert!(
                panel.contains(&peak),
                "width {width}: y axis lost its {peak} label"
            );
            assert!(
                panel.contains(&dash.events.days[0]),
                "width {width}: x axis lost its first day"
            );
        }
    }

    #[test]
    fn eased_settles_exactly_on_its_target() {
        let mut eased = Eased::new(1200.0);
        // Two seconds at the frame budget; anything still drifting after that
        // would leave the mining edge flickering forever.
        for _ in 0..40 {
            eased.step(FRAME.as_secs_f64());
        }
        assert_eq!(eased.shown, 1200.0);
        assert!(!eased.moving());
    }

    #[test]
    fn a_property_without_settings_falls_back_to_the_defaults() {
        let defaults = Settings {
            days: 7,
            refresh: 30,
            live_refresh: 3,
        };
        let bare = Property {
            id: "222".into(),
            ..Property::default()
        };
        let tuned = Property {
            id: "111".into(),
            days: Some(28),
            refresh: Some(120),
            ..Property::default()
        };

        let a = defaults.for_property(&tuned);
        assert_eq!((a.days, a.refresh, a.live_refresh), (28, 120, 3));

        // Switching to a bare property must land on the defaults, not inherit
        // the 28 days the previous property asked for.
        let b = defaults.for_property(&bare);
        assert_eq!((b.days, b.refresh, b.live_refresh), (7, 30, 3));
    }

    #[test]
    fn eased_is_framerate_independent() {
        let (mut fast, mut slow) = (Eased::new(100.0), Eased::new(100.0));
        for _ in 0..20 {
            fast.step(0.05);
        }
        for _ in 0..5 {
            slow.step(0.2);
        }
        // One second of motion either way lands in the same place.
        assert!((fast.shown - slow.shown).abs() < 0.5);
    }

    #[test]
    fn bars_always_fill_exactly_their_cells() {
        for step in 0..=20 {
            let frac = step as f64 / 20.0;
            for active in [false, true] {
                let width: usize = bar_spans(frac, 16, glyph::FULL, ore::grass(), 1.7, active, 2)
                    .iter()
                    .skip(1) // the indent
                    .map(|span| span.content.chars().count())
                    .sum();
                assert_eq!(width, 16, "frac {frac}, active {active}");
            }
        }
    }

    #[test]
    fn the_sweep_keeps_moving_after_a_bar_settles() {
        // `head` is the length of the span before the highlight, so a moving
        // sweep shows up as that span changing length over time.
        let head = |phase: f64| {
            bar_spans(1.0, 20, glyph::FULL, ore::grass(), phase, false, 0)[1]
                .content
                .chars()
                .count()
        };
        let settled: Vec<usize> = (0..12).map(|i| head(i as f64 * 0.25)).collect();
        assert!(
            settled.windows(2).any(|w| w[0] != w[1]),
            "a settled bar stopped animating: {settled:?}"
        );
    }

    #[test]
    fn a_small_terminal_gets_the_notice_not_a_broken_dashboard() {
        // Both numbers are named, and the short one is what the reader needs.
        let text = render_to_string(too_small(Rect::new(0, 0, 72, 18)));
        assert!(text.contains("72"), "missing actual width: {text:?}");
        assert!(text.contains("18"), "missing actual height: {text:?}");
        assert!(
            text.contains(&MIN_COLS.to_string()) && text.contains(&MIN_ROWS.to_string()),
            "missing what it needs: {text:?}"
        );
    }

    /// The left column with every panel asked for, which is the case the
    /// allocation is actually interesting in.
    fn all_three() -> Panels {
        Panels {
            events: true,
            map: true,
            vitals: true,
            ..capture_dash().panels
        }
    }

    fn placed(stack: &[(Stack, u16)], want: Stack) -> Option<u16> {
        stack
            .iter()
            .find(|(panel, _)| std::mem::discriminant(panel) == std::mem::discriminant(&want))
            .map(|(_, rows)| *rows)
    }

    #[test]
    fn hiding_one_panel_hides_one_panel() {
        // `5` used to close half the dashboard: the left column was drawn only
        // if the vitals were on, so turning the figures off took the chart and
        // the map with them and nothing in that half could be brought back
        // without them.
        let without_vitals = Panels {
            vitals: false,
            ..all_three()
        };
        assert!(
            without_vitals.left_any(),
            "the column went with the panel that was switched off"
        );

        let stack = left_column(&without_vitals, 40);
        assert!(
            placed(&stack, Stack::Events).is_some(),
            "the chart went too"
        );
        assert!(placed(&stack, Stack::Map).is_some(), "the map went too");
        assert!(
            placed(&stack, Stack::Vitals).is_none(),
            "the figures stayed"
        );

        // And the rows the figures were using are spent rather than left as a
        // gap: the map takes as many as it can draw, the chart takes the rest.
        assert_eq!(
            placed(&stack, Stack::Map),
            Some(MAP_MAX_ROWS),
            "the map kept its short box with rows going spare"
        );

        // The other two, the same way round.
        let only_vitals = Panels {
            events: false,
            map: false,
            ..all_three()
        };
        assert!(only_vitals.left_any());
        assert_eq!(
            left_column(&only_vitals, 40).len(),
            1,
            "a panel that is off claimed rows"
        );

        // Nothing left on that side, and the column is not drawn at all.
        let neither = Panels {
            events: false,
            map: false,
            vitals: false,
            ..all_three()
        };
        assert!(!neither.left_any());
        assert!(left_column(&neither, 40).is_empty());
    }

    #[test]
    fn the_map_only_takes_the_vitals_rows_when_the_vitals_are_gone() {
        // The regression the growth above could cause. At the size the site
        // captures, three rows moved to the map are three the figures needed to
        // get their bars back.
        const CAPTURE_BODY: u16 = 38;
        assert_eq!(
            placed(&left_column(&all_three(), CAPTURE_BODY), Stack::Map),
            Some(MAP_ROWS),
            "the map grew at the figures' expense"
        );
    }

    #[test]
    fn events_outranks_the_rest_when_the_column_is_short() {
        // Over every height the dashboard will draw at: events takes its box
        // first, and nothing else may claim rows it needed.
        for height in 0..=80u16 {
            let stack = left_column(&all_three(), height);
            let events = placed(&stack, Stack::Events).is_some();

            if height >= EVENTS_ROWS {
                assert!(events, "height {height}: events dropped while it fitted");
            }
            if (EVENTS_ROWS..EVENTS_ROWS + 1 + VITALS_MIN_ROWS).contains(&height) {
                assert_eq!(
                    stack.len(),
                    1,
                    "height {height}: something took rows events needed"
                );
            }
        }
    }

    #[test]
    fn a_column_with_room_for_one_of_them_draws_the_map() {
        // The order between the two: every number in the vitals is a
        // `craft overview` away, and the world is only ever drawn here. So a
        // column too short for both keeps the map — the panel that cannot be
        // had any other way — and not the table.
        let only_one = EVENTS_ROWS + 1 + MAP_ROWS;
        let room_for_both = only_one + 1 + VITALS_MIN_ROWS;

        for height in only_one..room_for_both {
            let stack = left_column(&all_three(), height);
            assert!(
                placed(&stack, Stack::Map).is_some(),
                "height {height}: the map lost to the figures"
            );
            assert!(
                placed(&stack, Stack::Vitals).is_none(),
                "height {height}: both claimed rows only one of them had"
            );
        }

        // Below that the map cannot be drawn at all, and the vitals are not
        // starved on behalf of a panel that was never going to fit.
        let stack = left_column(&all_three(), only_one - 1);
        assert!(placed(&stack, Stack::Map).is_none());
        assert!(placed(&stack, Stack::Vitals).is_some());
    }

    #[test]
    fn the_map_survives_by_shrinking_the_vitals() {
        // The regression this order exists for. A 132x52 terminal — the size
        // the site captures at — leaves the body 38 rows, and reserving the
        // vitals' full height spent 20 of them on a table with gaps in it and
        // left the map nothing. All three panels have to be up at that size.
        const CAPTURE_BODY: u16 = 38;
        let stack = left_column(&all_three(), CAPTURE_BODY);

        assert_eq!(stack.len(), 3, "a 132x52 dashboard dropped a panel");
        assert_eq!(
            placed(&stack, Stack::Vitals),
            Some(VITALS_MIN_ROWS),
            "vitals claimed more than the least it can be drawn in"
        );

        // Being last down the column, the vitals are then handed every row the
        // others left, so the densities come back as the terminal grows. These
        // are the two heights that matter: the bars return at 40 rows of body,
        // and the gaps between them at 46. Both were two rows later while the
        // chart was two rows taller than the map — every density in this column
        // came back sooner the day the two boxes were made the same size.
        let granted = |body: u16| body - (EVENTS_ROWS + 1 + 1 + MAP_ROWS);
        assert_eq!(granted(40), VITALS_TIGHT_ROWS, "the bars came back late");
        assert_eq!(granted(46), VITALS_ROWS, "the gaps came back late");

        // And the general rule: wherever there is room for the chart, the
        // vitals at their smallest and the map, the map is drawn.
        let room = EVENTS_ROWS + 1 + VITALS_MIN_ROWS + 1 + MAP_ROWS;
        for height in room..=80u16 {
            assert!(
                placed(&left_column(&all_three(), height), Stack::Map).is_some(),
                "height {height}: the map was dropped with the rows to draw it"
            );
        }
    }

    /// `BORING` is one global and tests run in parallel, so the tests that
    /// flip it take turns.
    static VOCAB: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Pins the vocabulary for a test: craft on acquire, craft again on drop.
    struct Vocab(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl Vocab {
        fn lock() -> Self {
            // The guarded data is `()`, so a poisoned lock is still good.
            let guard = VOCAB
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let held = Self(guard);
            held.craft();
            held
        }

        fn boring(&self) {
            if !theme::boring() {
                theme::toggle_boring();
            }
        }

        fn craft(&self) {
            if theme::boring() {
                theme::toggle_boring();
            }
        }
    }

    impl Drop for Vocab {
        fn drop(&mut self) {
            self.craft();
        }
    }

    /// The vitals as a reader sees them, at the width of a left column on a
    /// 120-column terminal.
    fn vitals_text() -> String {
        rendered(
            67,
            VITALS_ROWS,
            metrics_panel(&capture_dash(), 67, VITALS_ROWS),
        )
        .join("\n")
    }

    #[test]
    fn the_vitals_say_ga4_when_the_costume_is_off() {
        // A screenshot for somebody who has never heard of a villager.
        let vocab = Vocab::lock();

        let craft = vitals_text();
        assert!(craft.contains("VILLAGERS"), "no costume on:\n{craft}");

        vocab.boring();
        let plain = vitals_text();

        assert!(plain.contains("USERS"), "no plain name:\n{plain}");
        assert!(plain.contains("totalUsers"), "no GA4 field:\n{plain}");
        assert!(!plain.contains("VILLAGERS"), "costume still on:\n{plain}");
        // The panel is retitled too, or the screenshot still says VITALS.
        assert!(plain.contains("OVERVIEW"), "panel not retitled:\n{plain}");
    }

    #[test]
    fn the_costume_goes_back_on() {
        // `b` twice — flip, screenshot, flip back — lands on what was there.
        let vocab = Vocab::lock();

        let before = vitals_text();
        vocab.boring();
        assert_ne!(before, vitals_text(), "the toggle did nothing");

        vocab.craft();
        assert_eq!(before, vitals_text(), "the costume came back different");
    }

    /// Lives here rather than in `mcp`'s tests because `VOCAB` does: `BORING`
    /// is one global, and a second mutex in another module would not take
    /// turns with this one.
    #[test]
    fn what_an_assistant_reads_does_not_depend_on_the_vocabulary() {
        // The `b` key is the dashboard's costume, not a setting. Nothing in
        // `craft mcp` or `craft serve` touches it — but they share a binary
        // with the code that does, and a label that started going through
        // `Metric::label()` would quietly change the shape of every answer an
        // assistant gets. This is the assertion that would notice.
        let totals = [12481.0, 18203.0, 41776.0, 312.0, 0.41, 96.5];
        let prior = [11002.0, 16890.0, 38110.0, 270.0, 0.44, 91.2];
        // Rising, so an achievement is computed too rather than the list
        // coming back empty for a reason that has nothing to do with the
        // vocabulary.
        let daily: Vec<(String, f64)> = (1..=14)
            .map(|day| (format!("202609{day:02}"), 800.0 + day as f64 * 20.0))
            .collect();

        let vocab = Vocab::lock();
        let plain_spoken = crate::mcp::status_payload(&totals, &prior, &daily, false);
        vocab.boring();
        let boring = crate::mcp::status_payload(&totals, &prior, &daily, false);
        assert_eq!(plain_spoken, boring);

        // And it is data, not an empty shell that happens to match.
        assert_eq!(boring["has_data"], serde_json::json!(true));
        assert_eq!(
            boring["metrics"][0]["metric"],
            serde_json::json!("totalUsers")
        );
        assert_eq!(boring["metrics"][0]["value"], serde_json::json!(12481.0));
        assert_eq!(boring["daily_users"].as_array().map(Vec::len), Some(14));
        assert!(!boring["achievements"]
            .as_array()
            .expect("a list")
            .is_empty());
    }

    #[test]
    fn boring_mode_leaves_no_minecraft_word_on_screen() {
        // A new craft string that forgets its plain twin fails here.
        // `ANACRAFT` and its pickaxe are the product's name, not a costume,
        // so they are absent from the list below.
        use ratatui::{backend::TestBackend, Terminal};

        let vocab = Vocab::lock();
        vocab.boring();

        let dash = capture_dash();
        // The board tiles to fit, so a panel is only caught at a width that
        // draws it: desktop, the two published plates, the smallest board.
        for (width, height) in [(140, 48), CAPTURES[0], CAPTURES[1], (MIN_COLS, MIN_ROWS)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &dash)).unwrap();

            let buffer = terminal.backend().buffer().clone();
            let screen: String = (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");

            for word in [
                "VILLAGERS",
                "EXPEDITIONS",
                "BLOCKS MINED",
                "DIAMONDS",
                "CREEPER",
                "SURVIVED",
                "PORTALS",
                "realm",
                "spawned",
                "wandered",
                "shaft",
            ] {
                assert!(
                    !screen.contains(word),
                    "boring mode still says {word:?} at {width}x{height}:\n{screen}"
                );
            }
        }
    }

    #[test]
    fn the_vitals_give_up_their_gaps_before_their_bars() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;

        // Interior rows with any ink in them. Two per metric while the bars are
        // drawn, one per metric once they are given up — and the gaps never
        // count, being blank by definition.
        let inked = |height: u16| -> usize {
            let area = Rect::new(0, 0, 90, height);
            let mut buf = Buffer::empty(area);
            metrics_panel(&capture_dash(), area.width, height).render(area, &mut buf);
            (1..height - 1)
                .filter(|&y| (1..area.width - 1).any(|x| !buf[(x, y)].symbol().trim().is_empty()))
                .count()
        };

        let metrics = OVERVIEW.len();
        assert_eq!(inked(VITALS_ROWS), metrics * 2, "the full panel lost a row");
        assert_eq!(
            inked(VITALS_TIGHT_ROWS),
            metrics * 2,
            "closing the gaps cost a bar"
        );
        assert_eq!(
            inked(VITALS_MIN_ROWS),
            metrics,
            "the smallest panel is one line of numbers per metric"
        );

        // What separates full from tight is the blank the gaps leave behind:
        // the tight panel fills every row it was given.
        assert_eq!(
            inked(VITALS_TIGHT_ROWS),
            (VITALS_TIGHT_ROWS - 2) as usize,
            "the tight panel left dead rows inside its border"
        );
    }

    #[test]
    fn the_running_build_is_on_screen_whichever_side_you_are_on() {
        // Asked for twice a year and both times in a hurry: a release note
        // mentions something that is not there, or something broke and the
        // first question is which build. So it is on the one box that is always
        // drawn, in both of its states, rather than behind a key.
        let mut dash = capture_dash();
        let want = format!("v{}", env!("CARGO_PKG_VERSION"));

        for supporter in [false, true] {
            dash.supporter = supporter;
            let box_ = rendered(74, SUPPORTER_ROWS, supporter_box(&dash, 74)).join("\n");
            assert!(box_.contains(&want), "supporter {supporter}: {box_}");
        }
    }

    #[test]
    fn the_supporter_box_states_which_side_you_are_on() {
        // The ask and the thank-you are the same box, and neither state is
        // allowed to be silent — this is the line that pays for the rest.
        let mut dash = capture_dash();

        dash.supporter = false;
        let text = render_to_string(supporter_box(&dash, 132));
        assert!(text.contains("craft subscribe"), "no ask: {text:?}");
        assert!(
            text.contains("not an Anacrafter yet"),
            "no status: {text:?}"
        );

        dash.supporter = true;
        let text = render_to_string(supporter_box(&dash, 132));
        assert!(text.contains("ANACRAFTER"), "no status: {text:?}");
        assert!(!text.contains("craft subscribe"), "still asking: {text:?}");
    }

    #[test]
    fn a_list_panel_never_leaves_a_label_with_nothing_under_it() {
        // The column hands out spare rows one at a time and these panels spend
        // them two at a time — a heading and the bar beneath it — so an odd
        // row is the ordinary case rather than a rare one. Rendering into it
        // used to start a fourth entry and clip it, leaving a path with no bar
        // and no number under it, which reads as a panel that broke rather
        // than one that ran out of room.
        let mut dash = capture_dash();
        dash.apply_pages(vec![
            ("/".to_string(), 252.0),
            ("/setup-ga4.html".to_string(), 72.0),
            ("/pricing.html".to_string(), 55.0),
            ("/alerts.html".to_string(), 21.0),
        ]);

        // Nine rows: two borders and seven to draw in — three whole entries
        // and one row left over.
        let text = rendered(56, 9, pages_panel(&dash, 56, 9)).join("\n");
        assert!(
            text.contains("/pricing.html"),
            "third entry missing: {text}"
        );
        assert!(
            !text.contains("/alerts.html"),
            "started a row it could not finish: {text}"
        );

        // One row more is one whole entry more, and nothing is held back that
        // there was room for.
        let text = rendered(56, 11, pages_panel(&dash, 56, 11)).join("\n");
        assert!(text.contains("/alerts.html"), "row withheld: {text}");
    }

    #[test]
    fn a_column_too_short_for_the_vitals_still_spends_every_row() {
        // The reported symptom, from the bottom of the left column: on a
        // terminal one row too short for the vitals, the map inherited the
        // slack and drew nothing with it, so the box trailed off into ground
        // while the column beside it was full. The chart takes it now.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};

        let dash = capture_dash();
        let column = |height: u16| -> Vec<String> {
            let mut terminal = Terminal::new(TestBackend::new(88, height)).unwrap();
            terminal
                .draw(|frame| columns(frame, &dash, Rect::new(0, 0, 88, height), false))
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            (0..height)
                .map(|y| (0..88).map(|x| buffer[(x, y)].symbol()).collect::<String>())
                .collect()
        };

        // Events, a gutter, the map, a gutter, and one row fewer than the
        // vitals can open in — the shape that used to waste them.
        let height = EVENTS_ROWS + 1 + MAP_ROWS + 1 + VITALS_MIN_ROWS - 1;
        let rows = column(height);
        let at = |needle: &str| {
            rows.iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("{needle} was not drawn:\n{}", rows.join("\n")))
        };

        // The chart is taller than it asked for, which is where the rows the
        // vitals could not use went.
        // Strictly more than the box plus its gutter: at exactly that, the
        // chart is the height it asked for and the rows went somewhere else —
        // which was the bug.
        assert!(
            at("COUNTRIES") - at("EVENTS") > (EVENTS_ROWS + 1) as usize,
            "the slack did not reach the chart:\n{}",
            rows.join("\n")
        );

        // And the map still ends where the column does, so nothing trails off.
        let last = rows.len() - 1;
        assert!(
            rows[last].contains('\u{2570}') || rows[last].contains('\u{2514}'),
            "the column stops short of its own bottom:\n{}",
            rows.join("\n")
        );
    }

    #[test]
    fn the_wide_grid_runs_the_tiles_three_across_in_key_order() {
        // A 16:9-shaped body runs the tiles three across, filled in the order
        // the panels come. Every panel that is on gets a cell: all eight are
        // on here, so the board takes a third row rather than turning two of
        // them away.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};
        // Pinned to craft mode: it finds its tiles by `VITALS` and `PORTALS`.
        let _vocab = Vocab::lock();

        let dash = crowded_dash();
        let mut terminal = Terminal::new(TestBackend::new(132, 38)).unwrap();
        terminal
            .draw(|frame| body(frame, &dash, Rect::new(0, 0, 132, 38), false, true))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..38)
            .map(|y| {
                (0..132)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let at = |needle: &str| -> (usize, usize) {
            rows.iter()
                .enumerate()
                .find_map(|(y, row)| row.find(needle).map(|x| (x, y)))
                .unwrap_or_else(|| panic!("{needle} was not drawn"))
        };

        for needle in [
            "^2 RIGHT NOW",
            "^3 COUNTRIES",
            "^4 TOP PAGES",
            "^5 VITALS",
            "^6 TOP COUNTRIES",
        ] {
            at(needle);
        }

        // The rows are `KEY ORDER / 3`, so the first three tiles line up along
        // the top, each column's header further right than the last.
        let (x1, y1) = at("^1 EVENTS");
        let (x2, y2) = at("^2 RIGHT NOW");
        let (x3, y3) = at("^3 COUNTRIES");
        assert_eq!(y1, y2, "tiles did not line up on one row");
        assert_eq!(y1, y3, "tiles did not line up on one row");
        assert!(x1 < x2 && x2 < x3, "tiles are not three across");

        // The seventh and eighth are drawn too, on a row of their own beneath
        // the six — a panel that is on is a panel that is shown.
        let (_, y7) = at("^7 DAILY USERS");
        let (_, y8) = at("^8 PORTALS");
        assert_eq!(y7, y8, "the last two tiles are not sharing a row");
        assert!(y7 > y1, "the third row climbed onto the first");
    }

    #[test]
    fn the_board_opens_on_five_tiles() {
        // The dashboard opens on the first five keys, all five drawn and none
        // given up. It used to open on all eight, which put the board at its
        // most crowded before anyone had pressed anything. Six, seven and
        // eight wait behind their numbers.
        use ratatui::{backend::TestBackend, Terminal};
        // Pinned to craft mode: it finds its tiles by their craft headers.
        let _vocab = Vocab::lock();

        let dash = capture_dash();
        let on = &dash.panels;
        assert!(
            on.events && on.live && on.map && on.chunks && on.vitals,
            "the board did not open on the first five keys"
        );
        assert!(
            !on.realms_ranked && !on.trend && !on.portals,
            "the board opened on more than five tiles"
        );

        // And all five reach the screen — a default that drops a panel the
        // moment it is drawn is not a default.
        let (w, h) = (132u16, 40u16);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| draw(frame, &dash)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let screen: String = (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        for needle in [
            "^1 EVENTS",
            "^2 RIGHT NOW",
            "^3 COUNTRIES",
            "^4 TOP PAGES",
            "^5 VITALS",
        ] {
            assert!(screen.contains(needle), "{needle} did not reach the screen");
        }
        for needle in ["^6 TOP COUNTRIES", "^7 DAILY USERS", "^8 PORTALS"] {
            assert!(
                !screen.contains(needle),
                "{needle} was drawn without being asked for"
            );
        }
    }

    #[test]
    fn a_number_takes_its_panel_off_the_board_and_brings_it_back() {
        // The number in a tile's title is the key that fetches it. Pressing
        // it once takes the panel out of the active set and the survivors
        // re-tile over its space; pressing it again brings it back, at the
        // back of the board.
        let mut dash = capture_dash();
        assert!(dash.panels.events, "the board did not open on the chart");

        dash.toggle(Tile::Events);
        assert!(!dash.panels.events, "the number did not take the panel off");

        dash.toggle(Tile::Portals);
        assert!(dash.panels.portals, "the number did not bring a panel on");

        // Back on, and behind the ones that were already up — a panel that
        // returns joins the stack at the back, it does not reclaim its slot.
        dash.toggle(Tile::Events);
        assert!(
            dash.panels.events,
            "the number did not bring the panel back"
        );
        assert!(
            dash.joined[Tile::Events.index()] > dash.joined[Tile::Portals.index()],
            "the returning panel jumped back into its old slot"
        );
    }

    #[test]
    fn the_board_splits_rather_than_filling_the_widest_row_it_may() {
        // The first few panels split the body the way a tiling manager does.
        // One takes the whole of it, the second halves it, the third drops a
        // full-width row beneath the pair — it used to wedge itself in as a
        // third column, which left three panels on a wide terminal reading as
        // three narrow strips rather than a board — and the fourth joins that
        // row to make the 2×2.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};
        // Pinned to craft mode: it finds its tiles by their craft headers.
        let _vocab = Vocab::lock();

        let (w, h) = (160u16, 44u16);
        let mut dash = capture_dash();
        // The shape of the board, row by row: how many tiles each row carries,
        // top to bottom. Tiles on one row share a top border, so the corners
        // along a row count the tiles on it.
        let shape = |dash: &Dash| -> Vec<usize> {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|frame| body(frame, dash, Rect::new(0, 0, w, h), false, true))
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            (0..h)
                .map(|y| (0..w).filter(|&x| buffer[(x, y)].symbol() == "┌").count())
                .filter(|corners| *corners > 0)
                .collect()
        };

        let off = Panels {
            events: false,
            live: false,
            map: false,
            chunks: false,
            vitals: false,
            realms_ranked: false,
            trend: false,
            portals: false,
        };

        dash.panels = Panels {
            events: true,
            ..off
        };
        assert_eq!(shape(&dash), vec![1], "one panel did not take the body");

        dash.panels.live = true;
        assert_eq!(shape(&dash), vec![2], "two panels did not halve the body");

        dash.panels.map = true;
        assert_eq!(
            shape(&dash),
            vec![2, 1],
            "three panels did not put a full-width row under the pair"
        );

        dash.panels.chunks = true;
        assert_eq!(shape(&dash), vec![2, 2], "four panels did not make a 2×2");

        // Five is where a row first goes three across — the square the count
        // asks for is wider than the pair, and the body can pay for it.
        dash.panels.vitals = true;
        assert_eq!(
            shape(&dash),
            vec![3, 2],
            "five panels did not go three across"
        );
    }

    #[test]
    fn the_squarish_grid_earns_a_third_row() {
        // A 4:3-shaped body runs two tiles across, and its taller body earns
        // a third row — six tiles again, filled in key order: the vitals sit
        // third from the left on the bottom row, not by the chart.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};
        // Pinned to craft mode: it finds its tiles by `VITALS` and `PORTALS`.
        let _vocab = Vocab::lock();

        let dash = crowded_dash();
        let mut terminal = Terminal::new(TestBackend::new(100, 75)).unwrap();
        terminal
            .draw(|frame| body(frame, &dash, Rect::new(0, 0, 100, 75), false, false))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..75)
            .map(|y| {
                (0..100)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let at = |needle: &str| -> (usize, usize) {
            rows.iter()
                .enumerate()
                .find_map(|(y, row)| row.find(needle).map(|x| (x, y)))
                .unwrap_or_else(|| panic!("{needle} was not drawn"))
        };

        let (_, ey) = at("^1 EVENTS");
        let (_, my) = at("^3 COUNTRIES");
        let (_, vy) = at("^5 VITALS");

        // Two across: the map, third in order, leads the second row and the
        // vitals, fifth, the third — the taller body earned an extra row.
        assert!(my > ey, "the map did not reach the second row");
        assert!(vy > my, "the vitals did not reach the third row");

        // Two across and eight panels on is four rows, not three and a queue:
        // the last pair sits below the vitals rather than nowhere.
        let (_, py) = at("^8 PORTALS");
        assert!(py > vy, "the last row was never drawn");
    }

    #[test]
    fn a_4x3_terminal_runs_two_across_even_though_its_body_is_wide() {
        // The grid's shape is the terminal's, not the body's. A 4:3 terminal
        // leaves a body that has lost the header, box and footer, which by
        // itself would read as 16:9-wide — so measuring the body is how a 4:3
        // window ended up stuck three across. The flag comes from the whole
        // frame, and a squarish terminal draws two across.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};

        let dash = capture_dash();
        let mut terminal = Terminal::new(TestBackend::new(160, 104)).unwrap();
        terminal
            .draw(|frame| body(frame, &dash, Rect::new(0, 0, 160, 104), false, false))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..104)
            .map(|y| {
                (0..160)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let at = |needle: &str| -> (usize, usize) {
            rows.iter()
                .enumerate()
                .find_map(|(y, row)| row.find(needle).map(|x| (x, y)))
                .unwrap_or_else(|| panic!("{needle} was not drawn"))
        };

        // Right Now shares the chart's row — that's the top row of two tiles
        // — while the map waits its turn on the row beneath.
        let (_, y1) = at("^1 EVENTS");
        let (_, y2) = at("^2 RIGHT NOW");
        let (_, y3) = at("^3 COUNTRIES");
        assert_eq!(y1, y2, "the top row is not two across");
        assert!(y3 > y1, "the map climbed onto the top row");
    }

    #[test]
    fn the_grid_gives_up_the_last_tiles_under_pressure() {
        // The grid tops out at six tiles, so a 16:9 body short of the daily
        // chart and the portals is exactly the board it promised — the six
        // up front keep everything the queue let through.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};
        // Pinned to craft mode: it finds its tiles by `VITALS` and `PORTALS`.
        let _vocab = Vocab::lock();

        let dash = crowded_dash();
        let mut terminal = Terminal::new(TestBackend::new(132, 25)).unwrap();
        terminal
            .draw(|frame| body(frame, &dash, Rect::new(0, 0, 132, 25), false, true))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..25)
            .map(|y| {
                (0..132)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let drawn = |needle: &str| rows.iter().any(|row| row.contains(needle));

        assert!(drawn("^1 EVENTS") && drawn("^2 RIGHT NOW") && drawn("^3 COUNTRIES"));
        assert!(
            drawn("^4 TOP PAGES") && drawn("^5 VITALS") && drawn("^6 TOP COUNTRIES"),
            "the first two rows lost a tile"
        );
        assert!(
            !drawn("^7 DAILY USERS") && !drawn("^8 PORTALS"),
            "the last row outlived the room for it"
        );
    }

    #[test]
    fn the_grid_keeps_events_pinned_when_the_vitals_toggle() {
        // The tiles fill in the order the panels come, so the chart sits in
        // the first cell of the top row whatever joins it — switching the
        // vitals on fills the second row, and the chart stays where it was.
        // This is the movement the grid exists to avoid: a dashboard you keep
        // open should not rearrange itself because a neighbour joined the
        // board.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};

        let mut dash = crowded_dash();
        let at = |dash: &Dash| -> [(usize, usize); 3] {
            let mut terminal = Terminal::new(TestBackend::new(132, 38)).unwrap();
            terminal
                .draw(|frame| body(frame, dash, Rect::new(0, 0, 132, 38), false, true))
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            let rows: Vec<String> = (0..38)
                .map(|y| {
                    (0..132)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect();
            let at = |needle: &str| {
                rows.iter()
                    .enumerate()
                    .find_map(|(y, row)| row.find(needle).map(|x| (x, y)))
                    .unwrap_or_else(|| panic!("{needle} was not drawn"))
            };
            [at("^1 EVENTS"), at("^2 RIGHT NOW"), at("^3 COUNTRIES")]
        };

        let before = at(&dash);
        dash.panels.vitals = false;
        let after = at(&dash);

        assert_eq!(before, after, "toggling the vitals moved the top row");
    }

    #[test]
    fn the_right_column_covers_the_body_when_the_left_is_all_off() {
        // Nothing on the left homes — the chart, the map and the figures —
        // and the remaining panels fill the grid from the front of the queue;
        // none of the switched-off left is drawn, and none of the body is
        // left empty.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};

        let mut dash = capture_dash();
        dash.panels.events = false;
        dash.panels.map = false;
        dash.panels.vitals = false;
        let mut terminal = Terminal::new(TestBackend::new(100, 42)).unwrap();
        terminal
            .draw(|frame| body(frame, &dash, Rect::new(0, 0, 100, 42), false, true))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..42)
            .map(|y| {
                (0..100)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();

        assert!(
            rows.iter().any(|row| row.contains("^2 RIGHT NOW")),
            "the right column vanished"
        );
        assert!(
            rows.iter().all(|row| !row.contains("^1 EVENTS")),
            "the left drew a panel that was switched off"
        );
    }

    #[test]
    fn the_grid_fills_the_body_it_was_handed() {
        // The grid used to stop at whatever height its tiles could spend and
        // leave the rest as bare ground — four rows of it at the size the site
        // is captured at, sitting between the last panel and the supporter
        // box, which reads as the dashboard having stopped drawing. The last
        // row of tiles now ends on the body's last row, at every size.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};

        let dash = capture_dash();
        for (width, height) in [(132, 40), (204, 40), (100, 24), (150, 33), (74, 50)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    body(
                        frame,
                        &dash,
                        Rect::new(0, 0, width, height),
                        false,
                        width * 2 >= height * 3,
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            let inked = |y: u16| (0..width).any(|x| !buffer[(x, y)].symbol().trim().is_empty());
            assert!(
                inked(height - 1),
                "{width}x{height} left the body's last row bare"
            );
        }
    }

    #[test]
    fn a_narrow_body_gives_up_a_column_before_it_gives_up_the_reading() {
        // The aspect asks for columns; the width grants them. A 60-column
        // terminal is wider than it is tall, so it asked for three and got
        // tiles seventeen cells across with their headers cut mid-word. It
        // stacks instead, and a 74-column one — the width the site captures
        // its phone plate at — still manages two.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};

        let dash = capture_dash();
        let columns_at = |width: u16, height: u16| -> usize {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| body(frame, &dash, Rect::new(0, 0, width, height), false, true))
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            // Tiles on one row share a top border, so counting the corners
            // along the body's first row counts the columns.
            (0..width)
                .filter(|&x| buffer[(x, 0)].symbol() == "┌")
                .count()
        };

        assert_eq!(columns_at(60, 40), 1, "a 60-column body went multi-column");
        assert_eq!(
            columns_at(74, 58),
            2,
            "the phone plate lost its second tile"
        );
        assert_eq!(columns_at(132, 40), 3, "the desktop plate lost a column");
    }

    #[test]
    fn the_chart_and_the_map_take_the_wider_share_of_their_row() {
        // Both read across — a time series and a world map turn width into
        // detail where a list turns it into trailing space — so they get more
        // of the row than the panel between them.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};

        let dash = capture_dash();
        let mut terminal = Terminal::new(TestBackend::new(132, 40)).unwrap();
        terminal
            .draw(|frame| body(frame, &dash, Rect::new(0, 0, 132, 40), false, true))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        // The corners along the top row mark where each tile starts; the gaps
        // between them are the tile widths.
        let corners: Vec<u16> = (0..132)
            .filter(|&x| buffer[(x, 0)].symbol() == "┌")
            .collect();
        assert_eq!(corners.len(), 3, "the top row is not three across");
        // A gutter sits between the tiles, so it comes off the span between
        // one tile's corner and the next one's.
        let chart = corners[1] - corners[0] - 1;
        let feed = corners[2] - corners[1] - 1;
        assert!(
            chart > feed,
            "the chart ({chart}) did not outgrow the feed ({feed})"
        );
    }

    #[test]
    fn a_row_too_narrow_to_weight_splits_evenly_instead() {
        // The width a wide tile gains comes off its neighbour. On a phone-
        // sized row that would push the neighbour under MIN_TILE_COLS and cost
        // it its labels, which is worth more than the map gains — so the row
        // goes back to equal shares.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};

        let dash = capture_dash();
        let mut terminal = Terminal::new(TestBackend::new(74, 58)).unwrap();
        terminal
            .draw(|frame| body(frame, &dash, Rect::new(0, 0, 74, 58), true, false))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let corners: Vec<u16> = (0..74)
            .filter(|&x| buffer[(x, 0)].symbol() == "┌")
            .collect();
        assert_eq!(corners.len(), 2, "the phone plate is not two across");
        let chart = corners[1] - corners[0] - 1;
        let feed = 74 - corners[1];
        assert!(
            chart.abs_diff(feed) <= 1,
            "a 74-column row was split {chart}/{feed} rather than evenly"
        );
        assert!(
            chart >= MIN_TILE_COLS && feed >= MIN_TILE_COLS,
            "a tile was squeezed under the width it can be read at"
        );
    }

    #[test]
    fn a_phone_sized_terminal_gets_the_dashboard_not_the_notice() {
        // The site captures its narrow plate at 74 columns and the page shows
        // it to every visitor under 860 CSS pixels. The floor used to be 80,
        // so what phones were shown was the "too tight to work in" card — an
        // error screen advertised as the product.
        use ratatui::{backend::TestBackend, Terminal};

        let dash = capture_dash();
        for (width, height) in [(74, 58), (60, 24), (MIN_COLS, MIN_ROWS)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &dash)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            let screen: String = (0..height)
                .flat_map(|y| (0..width).map(move |x| (x, y)))
                .map(|(x, y)| buffer[(x, y)].symbol())
                .collect();
            assert!(
                !screen.contains("too tight to work in"),
                "{width}x{height} was shown the notice instead of the dashboard"
            );
            assert!(
                screen.contains("EVENTS"),
                "{width}x{height} drew no panel at all"
            );
        }
    }

    #[test]
    fn the_bars_shed_whole_pieces_rather_than_letting_the_border_cut_one() {
        // Every span used to be drawn whatever the room, and the block's
        // border cropped whatever hung over — which is how a 60-column
        // terminal came to say "◉ 130 on" and "· updated" with the clock
        // behind the wall. The three full-width bars give up whole pieces
        // instead, so what is left of one is true and none of them ends
        // flush against the frame.
        use ratatui::{backend::TestBackend, Terminal};

        let dash = capture_dash();
        for width in MIN_COLS..=200 {
            let mut terminal = Terminal::new(TestBackend::new(width, MIN_ROWS)).unwrap();
            terminal.draw(|frame| draw(frame, &dash)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            let lines: Vec<String> = (0..MIN_ROWS)
                .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();

            // The bars are the one-line boxes: the brand, the supporter box
            // and the hotbar. Each runs the full width, so each is the one
            // that can run into its own border.
            for mark in ["ANACRAFT ★", "ANACRAFTER", "[q]quit"] {
                let line = lines
                    .iter()
                    .find(|line| line.contains(mark))
                    .unwrap_or_else(|| panic!("no bar carrying {mark} at {width} columns"));
                let cells: Vec<&str> = line.split_inclusive(|_| true).collect();
                let close = cells
                    .iter()
                    .rposition(|cell| *cell == "│")
                    .expect("a bar with no closing border");
                assert!(
                    cells[close - 1] == " ",
                    "at {width} columns a bar ran up against its border: {line}"
                );
            }

            // The clock is the footer's first sacrifice, and it is never a
            // half of one.
            let hotbar = lines.iter().find(|line| line.contains("[q]quit")).unwrap();
            if hotbar.contains("updated") {
                assert!(
                    hotbar.contains(&dash.updated),
                    "at {width} columns the footer kept the word and lost the clock"
                );
            }
        }
    }

    #[test]
    fn a_panel_brought_back_joins_the_back_of_the_board() {
        // A tile's place is when it was switched on, not what its key number
        // is. From 1, 3, 8, hiding 3 leaves 1, 8 — and pressing 3 again puts
        // it at the back, giving 1, 8, 3. It used to shuffle back into the
        // middle as 1, 3, 8 and move the panel after it, which is not what
        // bringing a window back does.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};
        // Pinned to craft mode: it finds its tiles by `VITALS` and `PORTALS`.
        let _vocab = Vocab::lock();

        let (w, h) = (132u16, 40u16);
        let board = |dash: &Dash| -> Vec<String> {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|frame| body(frame, dash, Rect::new(0, 0, w, h), false, true))
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            let rows: Vec<String> = (0..h)
                .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();
            // The tiles, read the way a page is: top row first, left to right.
            let mut seen: Vec<(usize, usize, String)> = Vec::new();
            for (y, row) in rows.iter().enumerate() {
                for name in ["^1 EVENTS", "^3 COUNTRIES", "^8 PORTALS"] {
                    if let Some(x) = row.find(name) {
                        seen.push((y, x, name.to_string()));
                    }
                }
            }
            seen.sort();
            seen.into_iter().map(|(_, _, name)| name).collect()
        };

        let mut dash = capture_dash();
        dash.panels = Panels {
            events: true,
            map: true,
            portals: true,
            live: false,
            chunks: false,
            vitals: false,
            realms_ranked: false,
            trend: false,
        };
        assert_eq!(
            board(&dash),
            ["^1 EVENTS", "^3 COUNTRIES", "^8 PORTALS"],
            "an untouched board does not open in key order"
        );

        dash.toggle(Tile::Map);
        assert_eq!(
            board(&dash),
            ["^1 EVENTS", "^8 PORTALS"],
            "hiding 3 did not leave 1, 8"
        );

        dash.toggle(Tile::Map);
        assert_eq!(
            board(&dash),
            ["^1 EVENTS", "^8 PORTALS", "^3 COUNTRIES"],
            "3 shuffled back into the middle instead of joining at the back"
        );
    }

    #[test]
    fn switching_a_panel_on_makes_room_for_itself() {
        // Pressing 8 on a full board used to do nothing visible: the panel
        // came on and found no cell, because the board stopped at six tiles.
        // It pushes a row open for itself now, the way a new window does in a
        // tiling manager, and nothing already on the board is turned away to
        // pay for it.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};
        // Pinned to craft mode: it finds its tiles by `VITALS` and `PORTALS`.
        let _vocab = Vocab::lock();

        let (w, h) = (132u16, 40u16);
        let drawn = |dash: &Dash| -> Vec<String> {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|frame| body(frame, dash, Rect::new(0, 0, w, h), false, true))
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            (0..h)
                .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect())
                .collect()
        };
        let shows = |rows: &[String], needle: &str| rows.iter().any(|row| row.contains(needle));

        let mut dash = crowded_dash();
        dash.panels.portals = false;
        let before = drawn(&dash);
        assert!(
            !shows(&before, "^8 PORTALS"),
            "the eighth panel was drawn while switched off"
        );

        dash.panels.portals = true;
        let after = drawn(&dash);
        assert!(
            shows(&after, "^8 PORTALS"),
            "pressing 8 left the board exactly as it was"
        );

        // The seven that were already up are all still up.
        for needle in [
            "^1 EVENTS",
            "^2 RIGHT NOW",
            "^3 COUNTRIES",
            "^4 TOP PAGES",
            "^5 VITALS",
            "^6 TOP COUNTRIES",
            "^7 DAILY USERS",
        ] {
            assert!(
                shows(&after, needle),
                "{needle} was pushed off the board to make room for the eighth"
            );
        }

        // And the board still reaches the bottom of the body it was handed.
        assert!(
            !after[(h - 1) as usize].trim().is_empty(),
            "the board stopped short of the body's last row"
        );
    }

    #[test]
    fn switching_a_panel_off_re_tiles_the_rest_over_its_space() {
        // The board behaves like a tiling window manager: closing a window
        // does not leave its frame behind, the survivors re-split the space.
        // With 1 and 3 over 4 and 8 in a 2×2, switching 4 off leaves three,
        // which is a pair over a full-width row — and what is left is still a
        // full board, with no strip of bare ground where the panel used to be.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};
        // Pinned to craft mode: it finds its tiles by `VITALS` and `PORTALS`.
        let _vocab = Vocab::lock();

        let tiled = |dash: &Dash| -> Vec<String> {
            let (w, h) = (132u16, 40u16);
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal
                .draw(|frame| body(frame, dash, Rect::new(0, 0, w, h), false, true))
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            let rows: Vec<String> = (0..h)
                .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();

            // The tiling invariant: a row is either a gutter, which is bare
            // all the way across, or a row of tiles, which reaches both edges
            // of the body. Anything else is a hole.
            for (y, row) in rows.iter().enumerate() {
                if row.trim().is_empty() {
                    continue;
                }
                assert!(
                    !row.starts_with(' ') && !row.ends_with(' '),
                    "row {y} left bare ground at an edge: {row}"
                );
            }
            assert!(
                !rows[(h - 1) as usize].trim().is_empty(),
                "the board stopped short of the body's last row"
            );
            rows
        };

        let mut dash = capture_dash();
        dash.panels = Panels {
            events: true,
            map: true,
            chunks: true,
            portals: true,
            live: false,
            vitals: false,
            realms_ranked: false,
            trend: false,
        };
        let before = tiled(&dash);
        let row_of = |rows: &[String], needle: &str| -> usize {
            rows.iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("{needle} was not drawn"))
        };
        // Four panels make a 2×2: 1 and 3 along the top, 4 and 8 beneath
        // them, and no bare cell beside either pair.
        assert_eq!(
            row_of(&before, "^1 EVENTS"),
            row_of(&before, "^3 COUNTRIES"),
            "the first two panels are not sharing the top row"
        );
        assert_eq!(
            row_of(&before, "^4 TOP PAGES"),
            row_of(&before, "^8 PORTALS"),
            "the board ran three along the top and stretched the fourth"
        );
        assert!(
            row_of(&before, "^4 TOP PAGES") > row_of(&before, "^1 EVENTS"),
            "the board did not use its second row"
        );

        // Three left: 1 and 3 keep the top row and 8 takes the whole row
        // beneath, rather than a third column being wedged in beside them.
        dash.panels.chunks = false;
        let after = tiled(&dash);
        assert_eq!(
            row_of(&after, "^1 EVENTS"),
            row_of(&after, "^3 COUNTRIES"),
            "the pair did not keep the top row"
        );
        let below = row_of(&after, "^8 PORTALS");
        assert!(
            below > row_of(&after, "^1 EVENTS"),
            "the third panel squeezed into the row above instead of taking its own"
        );
        assert_eq!(
            after[below].matches('\u{250c}').count(),
            1,
            "the third panel did not take the full row"
        );
    }

    #[test]
    fn very_tall_windows_keep_the_two_columns() {
        // A body more than twice as tall as it is wide is where the grid gives
        // up and the columns cut back in — tiles that tall and narrow would be
        // unreadable. The chart, the map and the figures stack in the left
        // column the way they always did.
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};

        let dash = capture_dash();
        let mut terminal = Terminal::new(TestBackend::new(88, 220)).unwrap();
        terminal
            .draw(|frame| body(frame, &dash, Rect::new(0, 0, 88, 220), true, false))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..220)
            .map(|y| (0..88).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect();
        let at = |needle: &str| -> (usize, usize) {
            rows.iter()
                .enumerate()
                .find_map(|(y, row)| row.find(needle).map(|x| (x, y)))
                .unwrap_or_else(|| panic!("{needle} was not drawn"))
        };

        // The map sits under the chart in the same left column, not beside it
        // on a shared grid row.
        let (_, y1) = at("^1 EVENTS");
        let (_, y3) = at("^3 COUNTRIES");
        assert!(y3 > y1, "the map did not stack under the chart");
    }

    #[test]
    fn the_map_is_never_handed_rows_it_cannot_draw() {
        // `map_panel` samples the template down to the box and clamps at
        // `WORLD.len()`, so a taller box is a box with dead ground under the
        // caption — which is what the left column used to hand it whenever the
        // vitals did not fit. The slack goes to a panel that can spend it now,
        // and this is the number that decision is pinned to.
        assert_eq!(MAP_MAX_ROWS, WORLD.len() as u16 + 3);

        let dash = capture_dash();
        let world = |rows: Vec<String>| {
            rows.iter()
                .filter(|row| row.contains(LAND) || row.contains(glyph::FULL))
                .count()
        };

        let at_cap = world(rendered(
            74,
            MAP_MAX_ROWS,
            map_panel(&dash, 74, MAP_MAX_ROWS),
        ));
        let taller = world(rendered(
            74,
            MAP_MAX_ROWS + 6,
            map_panel(&dash, 74, MAP_MAX_ROWS + 6),
        ));

        assert_eq!(at_cap, WORLD.len(), "the map did not fill its own box");
        assert_eq!(
            taller, at_cap,
            "six more rows bought six more rows of nothing"
        );
    }

    #[test]
    fn the_portals_panel_splits_the_source_from_the_medium() {
        // GA hands back one string, "source / medium". The panel shows both —
        // the source is the name somebody would recognise, the medium is the
        // footnote saying how they arrived — so neither may be dropped.
        let mut dash = capture_dash();
        dash.portals = vec![
            ("news.ycombinator.com / referral".to_string(), 2_403.0),
            ("(direct) / (none)".to_string(), 4_496.0),
        ];

        let text = render_to_string(portals_panel(&dash, 56, PORTALS_ROWS));
        assert!(text.contains("news.ycombinator.com"), "no source: {text:?}");
        assert!(text.contains("referral"), "no medium: {text:?}");
        // Left as GA writes it. It is not a site, and dressing it up as one
        // would be the panel telling a small lie in its own vocabulary.
        assert!(text.contains("(direct)"), "direct was rewritten: {text:?}");

        // Ranked by sessions, so the busiest portal leads whatever order the
        // rows arrived in.
        let direct = text.find("(direct)").unwrap();
        let hn = text.find("news.ycombinator.com").unwrap();
        assert!(direct < hn, "the panel did not rank by sessions");
    }

    #[test]
    fn a_site_nobody_links_to_says_so() {
        // The empty state is a sentence, not a blank box: a new site with no
        // referrals looks exactly like a panel that failed to load.
        // Pinned to craft mode: the sentence has a boring twin now.
        let _vocab = Vocab::lock();
        let mut dash = capture_dash();
        dash.portals = Vec::new();
        let text = render_to_string(portals_panel(&dash, 56, PORTALS_ROWS));
        assert!(text.contains("nobody has sent anyone"), "silent: {text:?}");
    }

    #[test]
    fn the_anacrafter_wears_their_number() {
        // The number is part of the name on this line, so it goes beside the
        // word and nowhere else — and an account the service has no number for
        // reads exactly as it always did, rather than as a #000.
        let mut dash = capture_dash();
        dash.supporter = true;

        dash.founder = Some(41);
        let text = render_to_string(supporter_box(&dash, 132));
        assert!(text.contains("#041"), "no number: {text:?}");

        // Three digits, so the first hundred line up under each other.
        dash.founder = Some(7);
        assert!(
            render_to_string(supporter_box(&dash, 132)).contains("#007"),
            "the number was not padded"
        );

        // And past the padding it simply keeps counting.
        dash.founder = Some(1024);
        assert!(
            render_to_string(supporter_box(&dash, 132)).contains("#1024"),
            "the number was clipped"
        );

        dash.founder = None;
        let text = render_to_string(supporter_box(&dash, 132));
        assert!(
            text.contains("ANACRAFTER"),
            "the word went with it: {text:?}"
        );
        assert!(!text.contains('#'), "a number was invented: {text:?}");
    }

    #[test]
    fn the_demo_offers_the_anacrafter_look_without_claiming_it() {
        // The demo is the shop window for the subscription, so both states have
        // to name the key that switches between them — and the flattering one
        // has to say it is only a preview.
        let mut dash = capture_dash();
        dash.demo = true;

        dash.supporter = false;
        let text = render_to_string(supporter_box(&dash, 132));
        assert!(text.contains("craft subscribe"), "no ask: {text:?}");
        assert!(text.contains("to preview"), "no way in: {text:?}");

        dash.supporter = true;
        let text = render_to_string(supporter_box(&dash, 132));
        assert!(text.contains("ANACRAFTER"), "no status: {text:?}");
        assert!(text.contains("preview"), "reads as earned: {text:?}");

        // Off the demo, the star carries one of the subscriber lines and
        // nothing mentions a key.
        dash.demo = false;
        let text = render_to_string(supporter_box(&dash, 132));
        assert!(
            crate::license::SUPPORTER_LINES
                .iter()
                .any(|line| text.contains(line)),
            "no line for a subscriber: {text:?}"
        );
        assert!(!text.contains("preview"), "leaked the demo copy: {text:?}");
    }

    /// A paragraph's Debug repr, which embeds every span's content — enough to
    /// assert on what a widget says without standing up a terminal backend.
    /// Matches substrings only; it is not a layout assertion.
    fn render_to_string(p: Paragraph<'static>) -> String {
        format!("{p:?}")
    }

    #[test]
    fn the_avatar_stays_pinned_to_the_toolbar_edge() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;

        // The gap before the badge is a saturating subtraction, so a width the
        // chips had already eaten could push the face through the border or off
        // the end entirely. Every width the dashboard will actually draw at.
        let dash = settled_demo();
        let badge: Vec<char> = dash.avatar.glyphs().chars().collect();

        for width in MIN_COLS..=240 {
            let area = Rect::new(0, 0, width, 3);
            let mut buffer = Buffer::empty(area);
            header(&dash, width).render(area, &mut buffer);

            let right = width - 1;
            assert_eq!(
                buffer[(right, 1)].symbol(),
                "│",
                "the border went missing at width {width}"
            );
            assert_eq!(
                buffer[(right - AVATAR_INSET as u16, 1)].symbol(),
                " ",
                "the badge is touching the border at width {width}"
            );
            for (i, ch) in badge.iter().enumerate() {
                let x = right - AVATAR_INSET as u16 - avatar::CELLS + i as u16;
                assert_eq!(
                    buffer[(x, 1)].symbol(),
                    ch.to_string(),
                    "badge cell {i} is misplaced at width {width}"
                );
            }
        }
    }

    #[test]
    fn header_realms_are_dropped_whole_never_sliced() {
        let realms: Vec<(String, f64)> = vec![
            ("United States".into(), 44.0),
            ("India".into(), 18.0),
            ("Germany".into(), 12.0),
            ("United Kingdom".into(), 11.0),
            ("Brazil".into(), 8.0),
        ];
        for budget in 0..=64 {
            let chips = realm_chips(&realms, budget);
            let drawn: usize = chips
                .iter()
                .map(|(sep, chip)| sep.chars().count() + chip.chars().count())
                .sum();
            assert!(drawn <= budget, "budget {budget}: drew {drawn}");
            for (_, chip) in &chips {
                let count = chip.split(':').nth(1);
                assert!(
                    count.is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())),
                    "budget {budget}: truncated chip {chip:?}"
                );
            }
        }
        // Given room for everything, nothing is dropped.
        assert_eq!(realm_chips(&realms, 200).len(), 5);

        // The two "United ..." realms must not collapse onto the same label.
        let labels: Vec<String> = realm_chips(&realms, 200)
            .into_iter()
            .map(|(_, chip)| chip.split(':').next().unwrap().to_string())
            .collect();
        let unique: std::collections::HashSet<&String> = labels.iter().collect();
        assert_eq!(
            unique.len(),
            labels.len(),
            "ambiguous realm labels: {labels:?}"
        );
    }

    #[test]
    fn realm_abbreviations_distinguish_similar_names() {
        assert_eq!(realm_abbrev("United States"), "US");
        assert_eq!(realm_abbrev("United Kingdom"), "UK");
        assert_eq!(realm_abbrev("United Arab Emirates"), "UAE");
        assert_eq!(realm_abbrev("Bosnia and Herzegovina"), "BH");
        assert_eq!(realm_abbrev("India"), "Ind");
        assert_eq!(realm_abbrev("Germany"), "Ger");
    }

    #[test]
    fn the_map_separates_its_realms_from_its_land() {
        let dash = settled_demo();
        let rows = rendered(74, MAP_ROWS, map_panel(&dash, 74, MAP_ROWS));
        // The map's own rows, without the border and the caption under it.
        let map = &rows[1..rows.len() - 2];

        let land: usize = map.iter().map(|row| row.matches(LAND).count()).sum();
        let realms: usize = map.iter().map(|row| row.matches(glyph::FULL).count()).sum();

        // Land has to be one shade and the realms another, or the lit cells are
        // lost in the ground they sit on — which is what the two-shade terrain
        // this replaced did to them.
        assert_ne!(LAND, glyph::FULL.to_string());
        assert!(land > 100, "expected a drawn landmass, got {land} cells");
        assert!(
            realms > 0 && realms < land,
            "{realms} lit realms against {land} land cells"
        );
    }

    #[test]
    fn the_trend_caption_waits_for_the_first_day() {
        // `visible_days` never returns zero, so an empty history used to slice
        // from behind the front of the vec and take the whole dashboard down.
        assert_eq!(trend_caption(&[], 40), None);
        assert!(trend_caption(&[12.0], 40).unwrap().contains("1 days"));
    }

    #[test]
    fn the_daily_chart_fits_its_panel() {
        for width in 12..=60 {
            for days in [2, 7, 14, 30] {
                let values: Vec<f64> = (0..days).map(|d| 100.0 + d as f64).collect();
                for line in daily_chart(&values, width, 3) {
                    let drawn: usize = line
                        .spans
                        .iter()
                        .map(|span| span.content.chars().count())
                        .sum();
                    assert!(drawn <= width, "width {width}, {days} days: drew {drawn}");
                }
            }
        }
    }
}

#[cfg(test)]
mod forget_tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Draw the confirmation into a fixed buffer and read it back as text.
    fn render(width: u16, height: u16) -> String {
        let target = Forget {
            id: "397412345".to_string(),
            name: "example.com".to_string(),
        };
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                forget_overlay(frame, area, &target);
            })
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_confirmation_says_the_property_stays_in_google() {
        // The whole reason this overlay has more than one line. Somebody
        // confirming a box called FORGET PROPERTY in a dashboard would
        // otherwise assume their data went with it, and find out weeks later.
        let drawn = render(60, 20);
        assert!(drawn.contains("stays in Google"), "got:\n{drawn}");
        assert!(drawn.contains("Move to Trash Can"), "got:\n{drawn}");
        assert!(drawn.contains("FORGET PROPERTY"), "got:\n{drawn}");
        // Names what it is about to act on, not just "this property".
        assert!(drawn.contains("example.com"), "got:\n{drawn}");
        assert!(drawn.contains("397412345"), "got:\n{drawn}");
        // Both answers are offered; neither is implied by silence.
        assert!(drawn.contains("forget"), "got:\n{drawn}");
        assert!(drawn.contains("keep it"), "got:\n{drawn}");
    }

    #[test]
    fn the_confirmation_never_calls_itself_a_delete() {
        // It does not delete anything, and the word would be a lie the first
        // time somebody trusted it.
        let drawn = render(60, 20).to_lowercase();
        let claims_to_delete = drawn
            .lines()
            .filter(|line| line.contains("delet"))
            .all(|line| line.contains("console"));
        assert!(
            claims_to_delete,
            "the only mention of deleting must point at the console:\n{}",
            render(60, 20)
        );
    }

    #[test]
    fn the_confirmation_fits_a_small_terminal() {
        // An overlay wider than the terminal is how a confirmation ends up
        // unreadable at the moment it matters most.
        for (w, h) in [(40, 12), (48, 16), (60, 20), (120, 40), (24, 8)] {
            let drawn = render(w, h);
            for line in drawn.lines() {
                assert_eq!(
                    line.chars().count(),
                    w as usize,
                    "{w}x{h} drew a ragged line"
                );
            }
        }
    }
}

#[cfg(test)]
mod live_graph_tests {
    use super::*;

    fn plain(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The hero's own sample function, so the shapes are comparable.
    fn wave(n: usize) -> f64 {
        let n = n as f64;
        let v = 0.54
            + 0.26 * (n * 0.21).sin()
            + 0.13 * (n * 0.53 + 2.1).sin()
            + 0.07 * (n * 1.30 + 0.7).sin();
        v.clamp(0.06, 1.0) * 240.0
    }

    fn history(len: usize) -> VecDeque<f64> {
        (0..len).map(wave).collect()
    }

    #[test]
    fn the_graph_never_draws_past_its_panel() {
        // A row wider than the panel is a graph that corrupts every box to its
        // right, which is how a TUI layout breaks visibly.
        for width in 10..=80u16 {
            let columns = (width as usize).saturating_sub(5);
            for lines in [live_graph(&history(60), width, 3)] {
                for line in &lines {
                    let drawn: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
                    assert_eq!(drawn, columns + 2, "width {width} drew {drawn}");
                }
            }
        }
    }

    #[test]
    fn the_newest_sample_is_the_rightmost_column() {
        // A realtime graph that scrolls the wrong way is worse than no graph:
        // it reads as history rather than as arrival.
        let mut h: VecDeque<f64> = VecDeque::new();
        for _ in 0..30 {
            h.push_back(1.0);
        }
        h.push_back(100.0); // the newest, and by far the tallest
        let drawn = plain(&live_graph(&h, 40, 3));
        let top = drawn.lines().next().unwrap();
        assert_eq!(
            top.trim_end().chars().last(),
            Some(glyph::FULL),
            "the tall newest column is not at the right edge:\n{drawn}"
        );
    }

    #[test]
    fn a_short_history_fills_in_from_the_right() {
        // Stretching four polls across the panel would draw samples nobody
        // took. The chart grows into the space instead.
        let h: VecDeque<f64> = VecDeque::from(vec![10.0, 20.0, 30.0, 40.0]);
        let drawn = plain(&live_graph(&h, 40, 3));
        let bottom = drawn.lines().last().unwrap();
        assert!(
            bottom.starts_with("  ") && bottom[2..].starts_with(' '),
            "a four-sample history should sit at the right edge:\n{drawn}"
        );
        assert_eq!(bottom.trim_end().chars().count(), 37);
    }

    #[test]
    fn nothing_is_drawn_before_any_traffic() {
        // An all-zero history has no peak to scale against; drawing it would
        // divide by zero or paint a floor that means nothing.
        assert!(live_graph(&VecDeque::new(), 40, 3).is_empty());
        assert!(live_graph(&VecDeque::from(vec![0.0; 20]), 40, 3).is_empty());
        // And a panel too narrow to hold anything is not a panic.
        assert!(live_graph(&history(60), 4, 3).is_empty());
        assert!(live_graph(&history(60), 40, 0).is_empty());
    }

    #[test]
    fn only_the_cap_of_each_column_is_lit() {
        // The hero's rule, and the reason the chart reads as a chart: a dense
        // graph drawn solid and evenly coloured is a wall with a ragged top.
        // Exactly as many samples as columns, so there is no left padding to
        // index into by mistake.
        let h: VecDeque<f64> = VecDeque::from(vec![100.0; 15]);
        let lines = live_graph(&h, 20, 3);
        let colors: Vec<Vec<Option<Color>>> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .flat_map(|s| {
                        let color = s.style.fg;
                        s.content.chars().map(move |_| color)
                    })
                    .collect()
            })
            .collect();
        // Column 0 (flattened index 2, past the two-space indent) is a
        // full-height column that is not the newest: lit on top, dim below.
        assert_ne!(colors[0][2], colors[1][2], "the cap is not distinguished");
        assert_eq!(colors[1][2], colors[2][2], "the body is not one colour");
        // The newest column is the one arriving, and is lit whole.
        let last = colors[0].len() - 1;
        assert_eq!(
            colors[0][last], colors[1][last],
            "the newest column is not lit whole"
        );
        assert_ne!(
            colors[1][last], colors[1][2],
            "the newest column is not distinguished"
        );
    }
}

#[cfg(test)]
mod wired_tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(wired: &Wired, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                wired_overlay(frame, area, wired);
            })
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn wired() -> Wired {
        Wired {
            property: "example.com".to_string(),
            link: "http://127.0.0.1:7777/v1/mcp?token=abc123".to_string(),
            command: "claude mcp add --transport http anacraft \
                      'http://127.0.0.1:7777/v1/mcp?token=abc123'"
                .to_string(),
            trouble: None,
        }
    }

    #[test]
    fn the_overlay_shows_what_it_put_on_the_clipboard() {
        // OSC 52 is a request, and a terminal that ignores it says nothing.
        // The line has to be on screen to be selected by hand, or this key
        // silently does nothing on the terminals that decline.
        let drawn = render(&wired(), 80, 24);
        assert!(drawn.contains("MCP CONNECTOR"), "got:\n{drawn}");
        assert!(drawn.contains("claude mcp add"), "got:\n{drawn}");
        // Wrapped, so the halves may land on different lines — what matters
        // is that the whole link is on screen to be selected.
        assert!(drawn.contains("token="), "got:\n{drawn}");
        assert!(drawn.contains("abc123"), "got:\n{drawn}");
        // Which property, because an assistant pointed at the wrong site is
        // worse than one pointed at none.
        assert!(drawn.contains("example.com"), "got:\n{drawn}");
        // And the caveat: none of this answers unless the server is up.
        assert!(drawn.contains("craft serve"), "got:\n{drawn}");
    }

    #[test]
    fn nothing_to_copy_says_so_instead_of_showing_an_empty_box() {
        let mut broken = wired();
        broken.trouble = Some("no home directory to keep the secret in".into());
        let drawn = render(&broken, 80, 24);
        assert!(drawn.contains("no home directory"), "got:\n{drawn}");
        assert!(!drawn.contains("claude mcp add"), "got:\n{drawn}");
    }

    #[test]
    fn a_long_line_is_broken_rather_than_cut_off() {
        // Every piece fits, and putting them back together gives the line
        // that went to the clipboard — a wrap that drops a character hands
        // somebody a command that fails after they stop looking.
        let line = wired().command;
        let pieces = wrapped(&line, 60);
        assert!(pieces.iter().all(|piece| piece.chars().count() <= 60));
        assert_eq!(pieces.join(" "), line);
    }

    #[test]
    fn a_word_longer_than_the_box_is_cut_rather_than_overflowing() {
        // A token has no spaces in it. Wrapping on spaces alone would run it
        // off the side of the overlay and out of sight.
        let long = "a".repeat(25);
        let pieces = wrapped(&long, 10);
        assert_eq!(pieces, vec!["a".repeat(10), "a".repeat(10), "a".repeat(5)]);
    }
}
