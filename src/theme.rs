//! The texture pack: palettes, block glyphs, and the metric renaming table.
//!
//! Colors are looked up at render time rather than baked in as constants, so a
//! theme can be chosen on the command line or switched inside the running
//! dashboard. Everything the rest of the program draws with goes through the
//! ore names below, which map onto whichever palette is selected.

use std::sync::atomic::{AtomicUsize, Ordering};

use ratatui::style::Color;

/// A complete set of colors. Roles, not hues: `moss` is "the growing green
/// thing" in every theme, whatever that theme calls it.
pub struct Palette {
    /// Command-line name.
    pub name: &'static str,
    /// The ground everything sits on — the darkest shade.
    pub ink: Color,
    /// Panel interiors.
    pub bg: Color,
    /// Chrome (header and footer), one step above the panels.
    pub bg_lift: Color,
    /// Body text.
    pub fg: Color,
    /// Secondary text: labels, units, anything muted.
    pub sage: Color,
    /// Borders and unmined bar cells.
    pub shadow: Color,
    /// The signature color, reserved for what is alive right now.
    pub accent: Color,
    /// The accent with the lights off, for trailing or historical values.
    pub accent_deep: Color,
    /// The brightest note; use it sparingly, it reads as motion.
    pub bright: Color,
    pub moss: Color,
    pub leaf: Color,
    pub gold: Color,
    pub coral: Color,
    pub plum: Color,
    pub clay: Color,
    pub lapis: Color,
}

/// Deep jade ground, moss and teal midtones, one warm gold.
pub const OSAKA_JADE: Palette = Palette {
    name: "osaka-jade",
    ink: Color::Rgb(9, 16, 13),
    bg: Color::Rgb(17, 28, 24),
    bg_lift: Color::Rgb(24, 39, 33),
    fg: Color::Rgb(193, 196, 151),
    sage: Color::Rgb(108, 127, 115),
    shadow: Color::Rgb(58, 82, 71),
    accent: Color::Rgb(45, 213, 183),
    accent_deep: Color::Rgb(80, 148, 117),
    bright: Color::Rgb(126, 240, 208),
    moss: Color::Rgb(84, 158, 106),
    leaf: Color::Rgb(132, 183, 125),
    gold: Color::Rgb(255, 225, 107),
    coral: Color::Rgb(255, 83, 69),
    plum: Color::Rgb(210, 104, 156),
    clay: Color::Rgb(199, 137, 92),
    lapis: Color::Rgb(80, 148, 117),
};

/// Catppuccin Mocha.
pub const CATPPUCCIN: Palette = Palette {
    name: "catppuccin",
    ink: Color::Rgb(17, 17, 27),            // crust
    bg: Color::Rgb(30, 30, 46),             // base
    bg_lift: Color::Rgb(49, 50, 68),        // surface0
    fg: Color::Rgb(205, 214, 244),          // text
    sage: Color::Rgb(108, 112, 134),        // overlay0
    shadow: Color::Rgb(69, 71, 90),         // surface1
    accent: Color::Rgb(148, 226, 213),      // teal
    accent_deep: Color::Rgb(116, 199, 236), // sapphire
    bright: Color::Rgb(137, 220, 235),      // sky
    moss: Color::Rgb(166, 227, 161),        // green
    leaf: Color::Rgb(166, 227, 161),        // green
    gold: Color::Rgb(249, 226, 175),        // yellow
    coral: Color::Rgb(243, 139, 168),       // red
    plum: Color::Rgb(203, 166, 247),        // mauve
    clay: Color::Rgb(250, 179, 135),        // peach
    lapis: Color::Rgb(137, 180, 250),       // blue
};

/// GitHub's dark default.
pub const GITHUB: Palette = Palette {
    name: "github",
    ink: Color::Rgb(1, 4, 9),
    bg: Color::Rgb(13, 17, 23),
    bg_lift: Color::Rgb(22, 27, 34),
    fg: Color::Rgb(201, 209, 217),
    sage: Color::Rgb(139, 148, 158),
    shadow: Color::Rgb(48, 54, 61),
    accent: Color::Rgb(57, 197, 207),
    accent_deep: Color::Rgb(31, 111, 235),
    bright: Color::Rgb(86, 211, 100),
    moss: Color::Rgb(63, 185, 80),
    leaf: Color::Rgb(86, 211, 100),
    gold: Color::Rgb(210, 153, 34),
    coral: Color::Rgb(248, 81, 73),
    plum: Color::Rgb(188, 140, 255),
    clay: Color::Rgb(219, 109, 40),
    lapis: Color::Rgb(88, 166, 255),
};

/// Tokyo Night — deep navy ground, blue-violet accents, one warm gold.
pub const TOKYO_NIGHT: Palette = Palette {
    name: "tokyo-night",
    ink: Color::Rgb(22, 24, 35),
    bg: Color::Rgb(26, 27, 38),
    bg_lift: Color::Rgb(36, 40, 59),
    fg: Color::Rgb(192, 202, 245),
    sage: Color::Rgb(137, 143, 182),
    shadow: Color::Rgb(57, 63, 81),
    accent: Color::Rgb(122, 162, 247),
    accent_deep: Color::Rgb(137, 220, 235),
    bright: Color::Rgb(187, 154, 247),
    moss: Color::Rgb(158, 206, 106),
    leaf: Color::Rgb(115, 218, 202),
    gold: Color::Rgb(224, 175, 104),
    coral: Color::Rgb(247, 118, 142),
    plum: Color::Rgb(187, 154, 247),
    clay: Color::Rgb(255, 158, 100),
    lapis: Color::Rgb(125, 207, 255),
};

/// Solarized Dark
pub const SOLARIZED_DARK: Palette = Palette {
    name: "solarized-dark",
    ink: Color::Rgb(0, 31, 39),            // below base03
    bg: Color::Rgb(0, 43, 54),             // base03
    bg_lift: Color::Rgb(7, 54, 66),        // base02
    fg: Color::Rgb(131, 148, 150),         // base0
    sage: Color::Rgb(101, 123, 131),       // base00
    shadow: Color::Rgb(88, 110, 117),      // base01
    accent: Color::Rgb(42, 161, 152),      // cyan
    accent_deep: Color::Rgb(27, 120, 118), // cyan, pulled toward base03
    bright: Color::Rgb(211, 54, 130),      // magenta
    moss: Color::Rgb(133, 153, 0),         // green
    leaf: Color::Rgb(133, 153, 0),         // green
    gold: Color::Rgb(181, 137, 0),         // yellow
    coral: Color::Rgb(220, 50, 47),        // red
    plum: Color::Rgb(108, 113, 196),       // violet
    clay: Color::Rgb(203, 75, 22),         // orange
    lapis: Color::Rgb(38, 139, 210),       // blue
};

/// Solarized Light
pub const SOLARIZED_LIGHT: Palette = Palette {
    name: "solarized-light",
    ink: Color::Rgb(226, 219, 199),         // below base2
    bg: Color::Rgb(253, 246, 227),          // base3
    bg_lift: Color::Rgb(238, 232, 213),     // base2
    fg: Color::Rgb(101, 123, 131),          // base00
    sage: Color::Rgb(131, 148, 150),        // base0
    shadow: Color::Rgb(147, 161, 161),      // base1
    accent: Color::Rgb(42, 161, 152),       // cyan
    accent_deep: Color::Rgb(116, 191, 178), // cyan, pulled toward base3
    bright: Color::Rgb(211, 54, 130),       // magenta
    moss: Color::Rgb(133, 153, 0),          // green
    leaf: Color::Rgb(133, 153, 0),          // green
    gold: Color::Rgb(181, 137, 0),          // yellow
    coral: Color::Rgb(220, 50, 47),         // red
    plum: Color::Rgb(108, 113, 196),        // violet
    clay: Color::Rgb(203, 75, 22),          // orange
    lapis: Color::Rgb(38, 139, 210),        // blue
};

/// Catppuccin Latte — Catppuccin's light variant.
pub const LIGHT: Palette = Palette {
    name: "catppuccin-latte",
    ink: Color::Rgb(230, 233, 240),
    bg: Color::Rgb(250, 251, 254),
    bg_lift: Color::Rgb(240, 243, 248),
    fg: Color::Rgb(36, 41, 56),
    sage: Color::Rgb(110, 118, 129),
    shadow: Color::Rgb(200, 207, 218),
    accent: Color::Rgb(9, 105, 218),
    accent_deep: Color::Rgb(87, 141, 235),
    bright: Color::Rgb(31, 111, 135),
    moss: Color::Rgb(26, 127, 55),
    leaf: Color::Rgb(14, 124, 82),
    gold: Color::Rgb(153, 102, 0),
    coral: Color::Rgb(201, 42, 42),
    plum: Color::Rgb(130, 50, 203),
    clay: Color::Rgb(190, 75, 21),
    lapis: Color::Rgb(31, 88, 205),
};

pub const THEMES: [&Palette; 7] = [
    &OSAKA_JADE,
    &CATPPUCCIN,
    &GITHUB,
    &TOKYO_NIGHT,
    &LIGHT,
    &SOLARIZED_DARK,
    &SOLARIZED_LIGHT,
];

static SELECTED: AtomicUsize = AtomicUsize::new(0);

/// The palette in force. Every color accessor goes through here.
pub fn palette() -> &'static Palette {
    THEMES[SELECTED.load(Ordering::Relaxed) % THEMES.len()]
}

/// Selects a theme by name, returning false for a name nobody defines.
pub fn select(name: &str) -> bool {
    match THEMES.iter().position(|p| p.name == name) {
        Some(index) => {
            SELECTED.store(index, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// Steps to the next theme and returns its name — what the dashboard's `t` key
/// is wired to.
pub fn cycle() -> &'static str {
    let next = (SELECTED.load(Ordering::Relaxed) + 1) % THEMES.len();
    SELECTED.store(next, Ordering::Relaxed);
    THEMES[next].name
}

pub fn ink() -> Color {
    palette().ink
}
pub fn bg() -> Color {
    palette().bg
}
pub fn bg_lift() -> Color {
    palette().bg_lift
}
pub fn fg() -> Color {
    palette().fg
}
pub fn sage() -> Color {
    palette().sage
}
pub fn shadow() -> Color {
    palette().shadow
}
pub fn accent() -> Color {
    palette().accent
}
pub fn accent_deep() -> Color {
    palette().accent_deep
}
pub fn bright() -> Color {
    palette().bright
}

/// The ore names the rest of the program renders with, mapped onto whichever
/// palette is selected. Keeping the ore vocabulary means the texture pack
/// survives a theme swap.
#[allow(dead_code)] // full palette, not all shades used yet
pub mod ore {
    use super::palette;
    use ratatui::style::Color;

    pub fn grass() -> Color {
        palette().moss
    }
    pub fn dirt() -> Color {
        palette().clay
    }
    pub fn stone() -> Color {
        palette().sage
    }
    pub fn diamond() -> Color {
        palette().accent
    }
    pub fn gold() -> Color {
        palette().gold
    }
    pub fn redstone() -> Color {
        palette().coral
    }
    pub fn emerald() -> Color {
        palette().leaf
    }
    pub fn lapis() -> Color {
        palette().lapis
    }
    pub fn iron() -> Color {
        palette().fg
    }
    pub fn copper() -> Color {
        palette().clay
    }
    pub fn netherite() -> Color {
        palette().shadow
    }
    pub fn xp() -> Color {
        palette().bright
    }
    pub fn ender() -> Color {
        palette().plum
    }
}

/// Blend two colors. Non-RGB colors have no channels to mix, so they pass
/// through unchanged.
pub fn mix(from: Color, to: Color, t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (from, to) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let lerp = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
            Color::Rgb(lerp(r1, r2), lerp(g1, g2), lerp(b1, b2))
        }
        _ => from,
    }
}

/// Pull a color toward the palette's brightest value — used for the moving
/// highlight on a bar that is still filling.
pub fn brighten(color: Color, t: f64) -> Color {
    mix(color, bright(), t)
}

/// Push a color back toward the dashboard ground — used to fade old events.
pub fn fade(color: Color, t: f64) -> Color {
    mix(color, ink(), t)
}

/// Block glyphs used to build charts out of "blocks" rather than lines.
#[allow(dead_code)] // full glyph set
pub mod glyph {
    /// A placed block.
    pub const FULL: char = '█';
    /// Empty space in a bar — reads as unmined stone.
    pub const EMPTY: char = '░';
    /// Partially mined.
    pub const PARTIAL: char = '▓';
    /// First crack in a block — the lightest of the three break stages.
    pub const CRACKED: char = '▒';
    /// Gem marker, used for conversion bars.
    pub const GEM: char = '◆';
    /// Sparkline ramp, low to high.
    pub const SPARK: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    pub const PICKAXE: &str = "⛏";
    /// Worn by a subscriber's dashboard, next to the brand.
    pub const STAR: &str = "★";
    pub const BANNER: &str = "⚑";
    /// An unlit cell of the event feed's dot-matrix field.
    pub const UNLIT: char = '·';
    pub const UP: char = '▲';
    pub const DOWN: char = '▼';
    /// Live indicator, cycled to make the dot breathe.
    pub const PULSE: [char; 3] = ['○', '◉', '●'];
    /// Spinner shown while a fetch is in flight.
    pub const SPINNER: [char; 8] = ['⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'];
}

/// A GA4 metric dressed up in Minecraft clothes.
///
/// `api` is what we send to the Data API, `craft` is what we show, and `plain`
/// is kept visible next to it so the dashboard is still readable by someone who
/// just wants their numbers.
pub struct Metric {
    pub api: &'static str,
    pub craft: &'static str,
    pub plain: &'static str,
    /// Looked up rather than stored, so the metric follows the selected theme.
    pub color: fn() -> Color,
    pub glyph: char,
    pub kind: Kind,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// Whole number, rendered with thousands separators.
    Count,
    /// 0.0–1.0 from the API, rendered as a percentage.
    Ratio,
    /// Seconds, rendered as `4m 12s`.
    Duration,
}

pub const VILLAGERS: Metric = Metric {
    api: "totalUsers",
    craft: "VILLAGERS",
    plain: "users",
    color: ore::grass,
    glyph: glyph::FULL,
    kind: Kind::Count,
};

#[allow(dead_code)]
pub const NEW_SPAWNS: Metric = Metric {
    api: "newUsers",
    craft: "NEW SPAWNS",
    plain: "new users",
    color: ore::emerald,
    glyph: glyph::FULL,
    kind: Kind::Count,
};

pub const EXPEDITIONS: Metric = Metric {
    api: "sessions",
    craft: "EXPEDITIONS",
    plain: "sessions",
    color: ore::lapis,
    glyph: glyph::FULL,
    kind: Kind::Count,
};

pub const BLOCKS_MINED: Metric = Metric {
    api: "screenPageViews",
    craft: "BLOCKS MINED",
    plain: "page views",
    color: ore::copper,
    glyph: glyph::FULL,
    kind: Kind::Count,
};

/// GA4 renamed `conversions` to `keyEvents`; [`crate::ga`] falls back
/// automatically for properties still on the old name.
pub const DIAMONDS: Metric = Metric {
    api: "keyEvents",
    craft: "DIAMONDS",
    plain: "conversions",
    color: ore::diamond,
    glyph: glyph::GEM,
    kind: Kind::Count,
};

pub const CREEPER_RATE: Metric = Metric {
    api: "bounceRate",
    craft: "CREEPER RATE",
    plain: "bounce rate",
    color: ore::redstone,
    glyph: glyph::FULL,
    kind: Kind::Ratio,
};

pub const TIME_SURVIVED: Metric = Metric {
    api: "averageSessionDuration",
    craft: "TIME SURVIVED",
    plain: "avg. session",
    color: ore::gold,
    glyph: glyph::FULL,
    kind: Kind::Duration,
};

#[allow(dead_code)]
pub const PLAYERS_ONLINE: Metric = Metric {
    api: "activeUsers",
    craft: "PLAYERS ONLINE",
    plain: "active users",
    color: ore::xp,
    glyph: glyph::FULL,
    kind: Kind::Count,
};

/// The headline row on `anacraft overview`.
pub const OVERVIEW: &[&Metric] = &[
    &VILLAGERS,
    &EXPEDITIONS,
    &BLOCKS_MINED,
    &DIAMONDS,
    &CREEPER_RATE,
    &TIME_SURVIVED,
];

/// Dimension renames, used for table headers.
pub fn dimension_label(api: &str) -> &'static str {
    match api {
        "pagePath" => "PAGE",
        "pageTitle" => "PAGE TITLE",
        "landingPage" => "LANDING PAGE",
        "sessionSourceMedium" => "SOURCE",
        "sessionSource" => "SOURCE",
        "country" => "COUNTRY",
        "city" => "CITY",
        "deviceCategory" => "DEVICE",
        "browser" => "BROWSER",
        "date" => "DAY",
        _ => "DIMENSION",
    }
}

/// Rotating color ramp for table rows and multi-series charts, so a list of
/// countries or pages looks like a row of different ores. Every entry is a
/// distinct hue in all three palettes, which is why `leaf` is not among them —
/// some themes render it and `moss` the same.
pub fn ramp(i: usize) -> Color {
    ramp_of(palette(), i)
}

/// The ramp of a named palette rather than the selected one, so a theme can be
/// shown in its own colors while another is in force.
pub fn ramp_of(p: &Palette, i: usize) -> Color {
    let ramp = [
        p.accent, p.gold, p.clay, p.lapis, p.coral, p.plum, p.moss, p.bright,
    ];
    ramp[i % ramp.len()]
}
