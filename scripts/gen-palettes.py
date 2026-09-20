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


# Every page that carries the site chrome. A palette picked on the homepage has
# to survive a click through to a guide, so all of them get the blocks and the
# restore — `lovable.html` excepted, which styles itself and has no `:root` in
# the shared shape to hang them off.
PAGES = [
    "index.html", "audit.html", "mcp.html", "serve.html", "alerts.html",
    "setup-ga4.html", "pricing.html", "burn.html", "success.html",
    "privacy.html", "terms.html", "lovable.html",
]

# The stylesheet `craft serve` carries in the binary and serves at `/app.css`.
# It gets the blocks and nothing else: the palette it wears comes from the
# config file, written by the server that rendered the page, not from
# `localStorage` — the port changes every run and a preference stored against
# `127.0.0.1:52413` is a preference stored against an origin that will not
# exist tomorrow.
CARRIED = ["assets/serve.css"]

SWATCH_START, SWATCH_END = "/* swatches:start */", "/* swatches:end */"


def swatch(p):
    """One palette chip, painted in its own palette.

    The chips live in a footer styled by whichever palette is *in force*, so
    they cannot read their colours off `html[data-pal]` the way everything
    else does. The page used to borrow them at runtime by setting the
    attribute on the document, reading the computed value and putting it back
    — a probe that only worked because the page was one document that never
    navigated. Server-rendered, each chip is told its two colours here, out of
    the same `css()` numbers the blocks above are built from, so there is no
    second list to drift.
    """
    block = dict(re.findall(r"--([a-z-]+): ([^;]+);", css(p)))
    return (f'.pal[data-pal="{p["name"]}"] {{ '
            f'--swatch-ground: {block["ground"]}; '
            f'--swatch-accent: {block["jade"]}; }}')

RESTORE_START = "<!-- palette-restore:start -->"
RESTORE_END = "<!-- palette-restore:end -->"

# Runs in <head>, before anything paints. A page reached by clicking a link
# would otherwise render the default and repaint, which reads as a flash of
# the wrong theme rather than as a preference being honoured.
RESTORE = """<script>
  (function () {
    try {
      var p = localStorage.getItem('palette');
      if (p && p !== 'osaka-jade') { document.documentElement.dataset.pal = p; }
    } catch (e) {}
  })();
</script>"""


def splice(text, start, end, body, indent=""):
    head, rest = text.split(start, 1)
    _, tail = rest.split(end, 1)
    return f"{head}{start}\n{body}\n{indent}{end}{tail}"


def prepare(text, name):
    """Add the markers, and the variable the chip colour needs, if absent."""
    if START not in text:
        # Straight after `:root { ... }`, so a palette block overrides it by
        # specificity and by source order both. The indent is read off the
        # `:root` line rather than assumed: lovable.html is indented four.
        m = re.search(r"^([ \t]*):root\s*\{", text, re.M)
        if not m:
            sys.exit(f"{name}: no :root to hang the palettes off")
        pad = m.group(1)
        close = text.index(f"\n{pad}}}\n", m.end()) + len(f"\n{pad}}}\n")
        text = text[:close] + f"{pad}{START}\n{pad}{END}\n" + text[close:]
    if "--on-accent:" not in text:
        text = text.replace("    --mono:", "    --on-accent: #09100d;\n    --mono:", 1)
    # `--ink` was doing double duty as an inset background and as text on an
    # accent chip; those contradict on a light palette.
    text = text.replace("background: var(--jade); color: var(--ink);",
                        "background: var(--jade); color: var(--on-accent);")
    if RESTORE_START not in text and name not in CARRIED:
        text = text.replace("</style>\n", f"</style>\n{RESTORE_START}\n{RESTORE_END}\n", 1)
    return text


def main():
    blocks = [css(p) for p in parse() if p["name"] != DEFAULT]
    body = "\n".join(blocks)

    for name in PAGES:
        page = ROOT / "docs" / name
        if not page.exists():
            sys.exit(f"{name}: not found")
        text = prepare(page.read_text(), name)
        pad = re.search(r"^([ \t]*)" + re.escape(START), text, re.M).group(1)
        text = splice(text, START, END, body, indent=pad)
        text = splice(text, RESTORE_START, RESTORE_END, RESTORE)
        page.write_text(text)

    chips = "\n".join(swatch(p) for p in parse())

    for name in CARRIED:
        page = ROOT / name
        if not page.exists():
            sys.exit(f"{name}: not found")
        text = prepare(page.read_text(), name)
        pad = re.search(r"^([ \t]*)" + re.escape(START), text, re.M).group(1)
        text = splice(text, START, END, body, indent=pad)
        # The chips are the carried stylesheet's alone: `.pal` means something
        # else on the site, where it is a row in the palette showcase.
        text = splice(text, SWATCH_START, SWATCH_END, chips)
        page.write_text(text)

    print(
        f"  {len(blocks)} palettes into {len(PAGES)} pages "
        f"and {len(CARRIED)} carried, from src/theme.rs"
    )


if __name__ == "__main__":
    main()
