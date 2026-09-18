"""Render the osaka-jade 132x52 capture to docs/dash.png.

Solid blocks and box-drawing are painted as cell geometry rather than set as
glyphs. A terminal snaps those to the cell so they tile; a font draws them at
their natural advance, which leaves a hairline seam down every bar. The shade
characters stay glyphs — their stipple is the texture the map and the bar
troughs are drawn in, and flattening them to a tint loses it.
"""
import html as htmllib
import re

from PIL import Image, ImageDraw, ImageFont

import os

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# The Nerd Font is what the cell metrics assume. On a maintainer's machine it
# sits at the path below; release CI points ANACRAFT_FONT_DIR at a copy it
# downloads, so the same glyphs land in the same cells either way.
FONTS = os.environ.get(
    "ANACRAFT_FONT_DIR", "/home/slp/.local/share/fonts/JetBrainsMonoNerdFont"
)
# Only keep fallbacks that exist: a missing file raises when the font is loaded,
# and a runner has less installed than a workstation.
FALLBACK = tuple(
    p
    for p in (
        "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    )
    if os.path.exists(p)
)

COLS, ROWS = 132, 52
CW, CH = 16.8, 28.0
PADX, PADY = 36, 8
W, H = round(2 * PADX + COLS * CW), round(2 * PADY + ROWS * CH)
RADIUS = 26

rgb = lambda c: tuple(int(c[i:i + 2], 16) for i in (1, 3, 5))
INK = rgb("#09100d")

page = open(f"{ROOT}/docs/index.html").read()
pre = re.search(r'<pre class="dash" data-theme="osaka-jade">(.*?)</pre>', page, re.S).group(1)


def grid(layer, prop):
    """-> ROWS x COLS of (colour, char, bold)."""
    out = []
    for line in layer.split("\n"):
        cells = []
        for m in re.finditer(r'<b style="([^"]*)">(.*?)</b>', line, re.S):
            style, body = m.group(1), htmllib.unescape(re.sub(r"<[^>]+>", "", m.group(2)))
            col = re.search(rf"{prop}:(#[0-9a-f]{{6}})", style)
            bold = "font-weight:700" in style.replace(" ", "")
            for ch in body:
                cells.append((rgb(col.group(1)) if col else INK, ch, bold))
        cells += [(INK, " ", False)] * (COLS - len(cells))
        out.append(cells[:COLS])
    return out


bg = grid(re.search(r'<span class="lyr bgl"[^>]*>(.*?)</span>', pre, re.S).group(1), "background")
fg = grid(re.search(r'<span class="lyr fgl"[^>]*>(.*?)</span>', pre, re.S).group(1), "color")
assert len(bg) == ROWS and len(fg) == ROWS, (len(bg), len(fg))

img = Image.new("RGBA", (W, H), INK + (255,))
d = ImageDraw.Draw(img)
box = lambda c, r: (round(PADX + c * CW), round(PADY + r * CH),
                    round(PADX + (c + 1) * CW) - 1, round(PADY + (r + 1) * CH) - 1)

for r in range(ROWS):
    for c in range(COLS):
        d.rectangle(box(c, r), fill=bg[r][c][0])

LOWER = {"▁": 1 / 8, "▂": 2 / 8, "▄": 4 / 8, "▅": 5 / 8, "▆": 6 / 8}
CORNER = {"┌": (1, 1, 0, 1), "┐": (1, 0, 1, 1), "└": (0, 1, 0, 1), "┘": (0, 0, 1, 1)}  # down,right,left,up

reg = ImageFont.truetype(f"{FONTS}/JetBrainsMonoNerdFont-Regular.ttf", 28)
bold = ImageFont.truetype(f"{FONTS}/JetBrainsMonoNerdFont-Bold.ttf", 28)
fbs = [ImageFont.truetype(f, 26) for f in FALLBACK]
_a, _d = reg.getmetrics()
BASE = CH * _a / (_a + _d)
notdef = bytes(reg.getmask("\U0010FFFD"))

for r in range(ROWS):
    for c in range(COLS):
        colour, ch, is_bold = fg[r][c]
        if ch == " ":
            continue
        x0, y0, x1, y1 = box(c, r)
        mx, my = (x0 + x1) // 2, (y0 + y1) // 2
        if ch == "█":
            d.rectangle((x0, y0, x1, y1), fill=colour)
        elif ch == "▀":
            d.rectangle((x0, y0, x1, my), fill=colour)
        elif ch in LOWER:
            d.rectangle((x0, y1 - round((y1 - y0 + 1) * LOWER[ch]) + 1, x1, y1), fill=colour)
        elif ch in "─━":
            t = 3 if ch == "━" else 2
            d.rectangle((x0, my - t // 2, x1, my - t // 2 + t - 1), fill=colour)
        elif ch == "│":
            d.rectangle((mx - 1, y0, mx, y1), fill=colour)
        elif ch in CORNER:
            down, right, left, up = CORNER[ch]
            if down: d.rectangle((mx - 1, my - 1, mx, y1), fill=colour)
            if up:   d.rectangle((mx - 1, y0, mx, my), fill=colour)
            if right:d.rectangle((mx - 1, my - 1, x1, my), fill=colour)
            if left: d.rectangle((x0, my - 1, mx, my), fill=colour)
        else:
            f = bold if is_bold else reg
            if bytes(f.getmask(ch)) == notdef:
                f = next((fb for fb in fbs if bytes(fb.getmask(ch)) != notdef
                          and fb.getmask(ch).getbbox()), f)
                d.text(((x0 + x1) / 2, PADY + r * CH + BASE), ch, font=f, fill=colour, anchor="ms")
            else:
                d.text((x0, PADY + r * CH + BASE), ch, font=f, fill=colour, anchor="ls")

mask = Image.new("L", (W, H), 0)
ImageDraw.Draw(mask).rounded_rectangle([0, 0, W - 1, H - 1], RADIUS, fill=255)
img.putalpha(mask)
out = f"{ROOT}/docs/dash.png"
img.save(out)
print(f"wrote {out}", img.size)
