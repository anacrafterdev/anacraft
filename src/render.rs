//! Static terminal rendering for the one-shot commands. The live `dash`
//! command uses ratatui instead; this module writes plain ANSI so output stays
//! pipe- and redirect-friendly.

use std::io::IsTerminal;
use std::sync::OnceLock;

use ratatui::style::Color;

use crate::theme::{glyph, ore, Kind, Metric};

/// Truecolor is disabled when stdout isn't a TTY, when NO_COLOR is set,
/// when TERM says dumb, and always under test — so `anacraft pages > report.txt`
/// stays clean.
fn color_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        // Tests need to have bare strings to do their measuring checks
        // without ANSI escape sequences throwing them off
        if cfg!(test) {
            return false;
        }
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        if std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false) {
            return false;
        }
        std::io::stdout().is_terminal()
    })
}

pub fn paint(text: &str, color: Color) -> String {
    if !color_enabled() {
        return text.to_string();
    }
    painted(text, color)
}

/// The escape sequence `paint` emits when color is on — split out so tests
/// can assert on it without depending on the terminal they run in.
fn painted(text: &str, color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m"),
        _ => text.to_string(),
    }
}

pub fn dim(text: &str) -> String {
    if !color_enabled() {
        return text.to_string();
    }
    format!("\x1b[2m{text}\x1b[0m")
}

pub fn bold(text: &str) -> String {
    if !color_enabled() {
        return text.to_string();
    }
    format!("\x1b[1m{text}\x1b[0m")
}

/// Rough display width: enough to keep the banner's right edge aligned.
///
/// Note the U+2600..U+27BF block (which contains ⛏ and ⚑) is deliberately *not*
/// double-width — those default to text presentation without a VS16 selector,
/// and terminals render them in a single column.
fn width(s: &str) -> usize {
    s.chars()
        .map(|c| match c as u32 {
            0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF => 2,
            0x1F300..=0x1FAFF => 2,
            _ => 1,
        })
        .sum()
}

pub const PANEL_WIDTH: usize = 62;

/// Top rule of a panel: `╔═ ⛏  TITLE ══════╗`
pub fn panel_top(title: &str) -> String {
    let label = format!("╔═ {}  {} ", glyph::PICKAXE, title);
    let used = width(&label);
    let fill = PANEL_WIDTH.saturating_sub(used + 1);
    paint(&format!("{label}{}╗", "═".repeat(fill)), ore::netherite())
}

pub fn panel_bottom() -> String {
    paint(
        &format!("╚{}╝", "═".repeat(PANEL_WIDTH.saturating_sub(2))),
        ore::netherite(),
    )
}

/// Thousands separators without pulling in a formatting crate.
pub fn commas(value: f64) -> String {
    let rounded = value.round().abs() as u64;
    let digits = rounded.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if value < 0.0 {
        format!("-{out}")
    } else {
        out
    }
}

pub fn duration(seconds: f64) -> String {
    let total = seconds.round().max(0.0) as u64;
    match (total / 60, total % 60) {
        (0, s) => format!("{s}s"),
        (m, s) if m < 60 => format!("{m}m {s:02}s"),
        (m, _) => format!("{}h {:02}m", m / 60, m % 60),
    }
}

/// Format a raw API value according to the metric's kind.
pub fn value(metric: &Metric, raw: f64) -> String {
    match metric.kind {
        Kind::Count => commas(raw),
        // GA4 returns ratios as 0.0–1.0.
        Kind::Ratio => format!("{:.1}%", raw * 100.0),
        Kind::Duration => duration(raw),
    }
}

/// A block bar. `frac` is clamped to 0..=1; empty space reads as unmined stone.
pub fn bar(frac: f64, cells: usize, block: char, color: Color) -> String {
    let frac = frac.clamp(0.0, 1.0);
    let filled = (frac * cells as f64).round() as usize;
    let filled = filled.min(cells);
    let mined = block.to_string().repeat(filled);
    let stone = glyph::EMPTY.to_string().repeat(cells - filled);
    format!("{}{}", paint(&mined, color), dim(&stone))
}

/// Period-over-period change. For bounce rate, down is good — callers pass
/// `lower_is_better` so the arrow colors match intent.
pub fn delta(current: f64, previous: f64, lower_is_better: bool) -> String {
    if previous <= 0.0 {
        return String::new();
    }
    let change = (current - previous) / previous * 100.0;
    if !change.is_finite() || change.abs() < 0.5 {
        return dim("— flat");
    }
    let rising = change > 0.0;
    let good = rising != lower_is_better;
    let arrow = if rising { glyph::UP } else { glyph::DOWN };
    let color = if good {
        ore::emerald()
    } else {
        ore::redstone()
    };
    paint(&format!("{arrow}{:.0}%", change.abs()), color)
}

/// Sparkline over a daily series, drawn with block heights.
pub fn sparkline(values: &[f64], color: Color) -> String {
    paint(&spark_glyphs(values), color)
}

/// The sparkline without the ANSI, for the renderers that send a series
/// somewhere a terminal escape is literal text rather than a color — a Slack
/// message, a JSON field. Same glyph mapping as `sparkline`, which paints this.
pub fn spark_glyphs(values: &[f64]) -> String {
    if values.is_empty() {
        return String::new();
    }
    let max = values.iter().cloned().fold(f64::MIN, f64::max);
    let min = values.iter().cloned().fold(f64::MAX, f64::min);
    let span = (max - min).max(f64::EPSILON);

    values
        .iter()
        .map(|v| {
            let norm = (v - min) / span;
            let idx = ((norm * (glyph::SPARK.len() - 1) as f64).round() as usize)
                .min(glyph::SPARK.len() - 1);
            glyph::SPARK[idx]
        })
        .collect()
}

/// How full to draw a metric's bar.
///
/// Scaling every count against the largest one in the group makes small-but-
/// important metrics (conversions next to page views) round to an empty bar.
/// Instead each bar is scaled against its own previous period, with 50% headroom:
/// a flat period sits at two thirds, growth pushes toward full, a decline visibly
/// drops. Ratios keep their natural 0–100% scale.
pub fn bar_fraction(metric: &Metric, current: f64, previous: f64) -> f64 {
    match metric.kind {
        Kind::Ratio => current,
        _ if previous > 0.0 => current / (previous * 1.5),
        // No prior period to compare against; show a neutral two-thirds bar.
        _ if current > 0.0 => 2.0 / 3.0,
        _ => 0.0,
    }
}

/// One headline metric: name, value, delta, and a bar.
pub fn metric_block(metric: &Metric, current: f64, previous: f64) -> String {
    let plain_label = format!("{} ({})", metric.craft, metric.plain);
    let label = format!(
        "{} {}",
        bold(&paint(metric.craft, (metric.color)())),
        dim(&format!("({})", metric.plain))
    );

    // Pad on the uncolored text, then colorize: ANSI escapes have no display
    // width but do count toward format!'s width specifier.
    let pad = 34usize.saturating_sub(width(&plain_label));
    let shown = format!("{:>10}", value(metric, current));

    let lower_is_better = metric.api == "bounceRate";
    let change = delta(current, previous, lower_is_better);
    let frac = bar_fraction(metric, current, previous);

    format!(
        "  {label}{}{}  {change}\n  {}\n\n",
        " ".repeat(pad),
        bold(&shown),
        bar(frac, 24, metric.glyph, (metric.color)())
    )
}

/// A ranked table (top pages, sources, countries) drawn as ore rows.
pub fn ranked_table(rows: &[(String, f64)], unit: &str, label_cells: usize) -> String {
    let peak = rows.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max);
    let mut out = String::new();

    for (i, (name, value)) in rows.iter().enumerate() {
        let color = crate::theme::ramp(i);
        let mut label: String = name.chars().take(label_cells).collect();
        if width(name) > label_cells {
            label.pop();
            label.push('…');
        }
        let frac = if peak > 0.0 { value / peak } else { 0.0 };
        out.push_str(&format!(
            "  {} {:<width$}  {}  {:>9} {}\n",
            paint(&format!("{:>2}", i + 1), ore::stone()),
            label,
            bar(frac, 18, glyph::FULL, color),
            bold(&commas(*value)),
            dim(unit),
            width = label_cells,
        ));
    }
    out
}

/// Achievement toast, Minecraft-style.
pub fn toast(title: &str, detail: &str) -> String {
    format!(
        "  {} {}\n    {}\n",
        paint(glyph::BANNER, ore::gold()),
        bold(&paint("ACHIEVEMENT GET!", ore::gold())),
        format_args!(
            "{} — {}",
            paint(&format!("\"{title}\""), ore::xp()),
            dim(detail)
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{DIAMONDS, TIME_SURVIVED, VILLAGERS};

    // color_enabled() has a check for cfg!(test) to ensure color is
    // disabled for tests

    #[test]
    fn tests_never_see_color() {
        assert!(!color_enabled(), "color leaked into the test run!");
    }

    #[test]
    fn painting_wraps_the_text_in_an_sgr_sequence() {
        assert_eq!(
            painted("x", Color::Rgb(1, 2, 3)),
            "\x1b[38;2;1;2;3mx\x1b[0m"
        );
        // Every palette is truecolor and anything else passes through unstyled
        assert_eq!(painted("x", Color::Green), "x");
    }

    #[test]
    fn commas_groups_thousands() {
        assert_eq!(commas(0.0), "0");
        assert_eq!(commas(999.0), "999");
        assert_eq!(commas(1000.0), "1,000");
        assert_eq!(commas(41776.4), "41,776");
        assert_eq!(commas(1234567.0), "1,234,567");
        assert_eq!(commas(-2500.0), "-2,500");
    }

    #[test]
    fn duration_reads_as_time() {
        assert_eq!(duration(45.0), "45s");
        assert_eq!(duration(214.0), "3m 34s");
        assert_eq!(duration(3725.0), "1h 02m");
    }

    #[test]
    fn ratio_metrics_render_as_percent() {
        // GA4 hands back 0.0-1.0 for rate metrics.
        assert_eq!(value(&crate::theme::CREEPER_RATE, 0.412), "41.2%");
        assert_eq!(value(&VILLAGERS, 12481.0), "12,481");
        assert_eq!(value(&TIME_SURVIVED, 214.0), "3m 34s");
    }

    #[test]
    fn bars_never_exceed_their_cell_count() {
        for frac in [-1.0, 0.0, 0.5, 1.0, 9.9] {
            let drawn = bar(frac, 24, glyph::FULL, ore::grass());
            assert_eq!(drawn.chars().count(), 24, "frac {frac} broke the bar width");
        }
    }

    #[test]
    fn small_metrics_still_get_a_visible_bar() {
        // The bug this guards: scaling every count against the largest metric
        // in the group rounded conversions (312) next to page views (41,776)
        // down to an empty bar.
        let frac = bar_fraction(&DIAMONDS, 312.0, 258.0);
        assert!(frac > 0.5, "small-but-growing metric collapsed to {frac}");

        let drawn = bar(frac, 24, DIAMONDS.glyph, (DIAMONDS.color)());
        assert!(
            drawn.contains(DIAMONDS.glyph),
            "conversions bar rendered empty"
        );
    }

    #[test]
    fn flat_period_sits_at_two_thirds() {
        let frac = bar_fraction(&VILLAGERS, 1000.0, 1000.0);
        assert!((frac - 2.0 / 3.0).abs() < 0.01, "flat period drew {frac}");
    }

    #[test]
    fn ratio_bars_use_their_own_absolute_scale() {
        // A 41% bounce rate should fill 41% of the bar regardless of history.
        let frac = bar_fraction(&crate::theme::CREEPER_RATE, 0.412, 0.478);
        assert!((frac - 0.412).abs() < f64::EPSILON);
    }

    #[test]
    fn delta_flips_arrow_colour_for_lower_is_better() {
        // Bounce rate falling is good news; users falling is not.
        assert!(delta(0.41, 0.48, true).contains(glyph::DOWN));
        assert!(delta(1200.0, 1000.0, false).contains(glyph::UP));
        assert_eq!(delta(100.0, 0.0, false), "", "no baseline means no delta");
        assert!(delta(1000.0, 1000.0, false).contains("flat"));
    }

    #[test]
    fn sparkline_length_matches_input() {
        let line = sparkline(&[1.0, 5.0, 3.0, 9.0], ore::grass());
        assert_eq!(line.chars().count(), 4);
        // A flat series must not divide by zero.
        assert_eq!(sparkline(&[7.0, 7.0, 7.0], ore::grass()).chars().count(), 3);
    }

    #[test]
    fn panel_rules_are_the_same_width() {
        let top = panel_top("REDSTONE LABS · last 7 days");
        assert_eq!(
            width(&top),
            width(&panel_bottom()),
            "panel top and bottom rules drifted apart"
        );
    }
}
