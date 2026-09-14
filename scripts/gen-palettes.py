#!/usr/bin/env python3
"""Generate the site's CSS palettes from the ones the binary ships.

The homepage lets a visitor pick a palette from the capture tabs and restyles
itself in it. That is seven sets of CSS variables, and hand-keeping them beside
`src/theme.rs` is how the light theme came to show a Catppuccin capture in a
Solarized frame. So they are derived from the Rust, once, by role:

    --ground     ink          the page
    --ink        bg           insets and cards, one step up from the page
    --panel      bg_lift      chrome
    --fg         fg           body text
    --white      derived      the highest-contrast text: fg pushed away from
                              the ground, since no palette carries one
    --dim        sage         muted text
    --jade       accent       the signature colour
    --jade-deep  accent_deep  the second accent
    --gold       gold
    --coral      coral
    --line/-2    shadow       borders, at two alphas
    --on-accent  derived      text on a jade chip: the ground for a dark
                              palette, the panel for a light one, because a
                              light palette's accent is dark enough to need
                              light text on it

osaka-jade is deliberately absent: it is `:root`, hand-tuned, and the page
already looks the way it should in it. Regenerating it would restyle the
default for the sake of symmetry.

Run through `make palettes`.
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DEFAULT = "osaka-jade"
START, END = "/* palettes:start */", "/* palettes:end */"


def rgb(hexish):
    h = hexish.lstrip("#")
    return tuple(int(h[i:i + 2], 16) for i in (0, 2, 4))


def mix(a, b, t):
    """a toward b by t, as #rrggbb."""
    return "#%02x%02x%02x" % tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def luma(c):
    r, g, b = c
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def _lin(c):
    c /= 255
    return c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4


def rel(c):
    r, g, b = c
    return 0.2126 * _lin(r) + 0.7152 * _lin(g) + 0.0722 * _lin(b)


def ratio(a, b):
    la, lb = rel(a), rel(b)
    hi, lo = max(la, lb), min(la, lb)
    return (hi + 0.05) / (lo + 0.05)


def legible(color, ground, target=4.5):
    """Push `color` away from `ground` until it clears `target`, or give up.

    A terminal palette is not a web palette. Solarized Light's `sage` is base0,
    a light grey chosen to sit on a dark terminal; as muted text on the light
    ground of a web page it reads at 2.3:1, which is not text, it is a hint of
    text. The same is true of an accent picked to glow on black.

    So the roles that carry words get nudged toward the far end of the ground
    they land on until they are legible. The hue survives; only the lightness
    moves, and only as far as it has to.
    """
    away = (0, 0, 0) if luma(ground) > 128 else (255, 255, 255)
    best = color
    for step in range(0, 101, 2):
        cand = rgb(mix(color, away, step / 100))
        best = cand
        if ratio(cand, ground) >= target:
            break
    return "#%02x%02x%02x" % best


def parse():
    src = (ROOT / "src" / "theme.rs").read_text()
    order = re.search(r"pub const THEMES: \[&Palette; \d+\] = \[(.*?)\];", src, re.S)
    names = re.findall(r"&([A-Z_]+),", order.group(1))

    pals = {}
    for block in re.finditer(r"pub const ([A-Z_]+): Palette = Palette \{(.*?)\n\};", src, re.S):
        const, body = block.group(1), block.group(2)
        fields = {m.group(1): "#%02x%02x%02x" % tuple(int(m.group(i)) for i in (2, 3, 4))
                  for m in re.finditer(r"(\w+):\s*Color::Rgb\((\d+),\s*(\d+),\s*(\d+)\)", body)}
        fields["name"] = re.search(r'name:\s*"([a-z-]+)"', body).group(1)
        pals[const] = fields
    return [pals[c] for c in names]


def css(p):
    ground, panel = rgb(p["ink"]), rgb(p["bg"])
    light = luma(ground) > 128
    # No palette carries a "brightest text", so it is fg pushed away from the
    # ground: toward white on a dark palette, toward black on a light one.
    white = mix(rgb(p["fg"]), (255, 255, 255) if not light else (0, 0, 0), 0.55)
    sr, sg, sb = rgb(p["shadow"])

    # The three roles that carry words have to clear a contrast floor against
    # the ground they land on; the rest are decoration and keep their value.
    fg = legible(rgb(p["fg"]), ground, 7.0)
    dim = legible(rgb(p["sage"]), ground, 4.5)
    jade = legible(rgb(p["accent"]), ground, 4.5)
    deep = legible(rgb(p["accent_deep"]), ground, 4.5)

    # Text on a jade chip: whichever end of the palette reads better on the
    # accent that survived the nudge above.
    on_accent = max((p["ink"], p["bg"]), key=lambda c: ratio(rgb(c), rgb(jade)))

    return "\n".join([
        f'  html[data-pal="{p["name"]}"] {{',
        f'    --ground: {p["ink"]};',
        f'    --ink: {p["bg"]};',
        f'    --panel: {p["bg_lift"]};',
        f'    --line: rgba({sr},{sg},{sb},.35);',
        f'    --line-2: rgba({sr},{sg},{sb},.55);',
        f'    --fg: {fg};',
        f'    --white: {white};',
        f'    --dim: {dim};',
        f'    --jade: {jade};',
        f'    --jade-deep: {deep};',
        f'    --gold: {p["gold"]};',
        f'    --coral: {p["coral"]};',
        f'    --on-accent: {on_accent};',
        '  }',
    ])


def main():
    blocks = [css(p) for p in parse() if p["name"] != DEFAULT]
    body = "\n".join(blocks)

    page = ROOT / "docs" / "index.html"
    text = page.read_text()
    if START not in text or END not in text:
        sys.exit(f"{page.name}: missing {START} / {END} markers")
    head, rest = text.split(START, 1)
    _, tail = rest.split(END, 1)
    page.write_text(f"{head}{START}\n{body}\n  {END}{tail}")
    print(f"  index.html: {len(blocks)} palettes generated from src/theme.rs")


if __name__ == "__main__":
    main()
