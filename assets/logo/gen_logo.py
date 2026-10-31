# SPDX-License-Identifier: GPL-3.0-only
"""Generates Kickstand's swinging icon and social preview, with Space Grotesk text converted to paths. The
wordmark itself is gen_wordmark.py's.

Usage: python gen_logo.py <SpaceGrotesk[wght].ttf> <out dir>  (needs fonttools and shapely; the font is
google/fonts ofl/spacegrotesk)
"""
import sys

from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont
from fontTools.varLib import instancer

import gen_wordmark

FONT_PATH, OUT = sys.argv[1], sys.argv[2]

INK, PAPER, RED, TILE = "#17191E", "#F2F0EA", "#FF4554", "#15171C"
GH_LIGHT_BG, GH_DARK_BG = "#FFFFFF", "#0D1117"


def load(weight):
    return instancer.instantiateVariableFont(TTFont(FONT_PATH), {"wght": weight})


FONTS = {500: load(500)}


def text_paths(font, text, size, x, baseline, tracking=0.0):
    """Returns (svg path d, end x) for `text` set at `size` px with its baseline at `baseline`."""
    upm = font["head"].unitsPerEm
    scale = size / upm
    cmap = font.getBestCmap()
    gs = font.getGlyphSet()
    hmtx = font["hmtx"]
    parts = []
    pen_x = x
    for ch in text:
        name = cmap[ord(ch)]
        pen = SVGPathPen(gs)
        gs[name].draw(TransformPen(pen, (scale, 0, 0, -scale, pen_x, baseline)))
        parts.append(pen.getCommands())
        pen_x += hmtx[name][0] * scale + tracking
    return " ".join(p for p in parts if p), pen_x - tracking


def write(name, body, w, h, label, note="Letters: Space Grotesk (SIL OFL 1.1), as outlines."):
    with open(f"{OUT}/{name}", "w") as f:
        f.write(
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0f}" height="{h:.0f}" viewBox="0 0 {w:.2f} {h:.2f}" '
            f'role="img" aria-label="{label}">\n'
            f"<!-- SPDX-License-Identifier: GPL-3.0-only. {note} -->\n"
            f"{body}\n</svg>\n"
        )


# The swinging icon: the mark on a rounded dark tile.
mark = (
    '<rect x="50" y="28" width="30" height="144" rx="7" fill="#F2F0EA"/>'
    '<g transform="translate(80 104)">'
    '<rect x="-15" y="-100" width="30" height="100" rx="7" fill="#F2F0EA" transform="rotate(42)"/>'
    '<g transform="rotate(-42)">'
    '<animateTransform attributeName="transform" type="rotate" values="-42;-42;-62;-36;-44;-42;-42" '
    'keyTimes="0;0.5;0.6;0.7;0.78;0.85;1" dur="3.2s" repeatCount="indefinite"/>'
    f'<rect x="-15" y="0" width="30" height="96" rx="7" fill="{RED}"/>'
    '<rect x="-15" y="84" width="30" height="12" rx="5" fill="#000000" fill-opacity="0.28"/>'
    f'<circle cx="0" cy="11" r="4.5" fill="{TILE}"/>'
    "</g></g>"
)
write(
    "kickstand-swing.svg",
    f'<rect width="256" height="256" rx="56" fill="{TILE}"/><g transform="translate(28 28)">{mark}</g>',
    256,
    256,
    "Kickstand logo: a K whose red lower leg is the Switch kickstand, kicking open",
)

# Social preview, 1280x640: the wordmark (gen_wordmark.py's, drawn at 128 px) and tagline centred on the
# dark tile colour.
W, H = 1280, 640
size = 168
s = size / gen_wordmark.SIZE
body, end = gen_wordmark.build(TILE, PAPER)
pad, asc = gen_wordmark.PAD, gen_wordmark.ASC
wm_w = (end - 2 * pad) * s
tag = "Deploy, run, and debug homebrew with AI"
tsize = 44
_, tend = text_paths(FONTS[500], tag, tsize, 0, 0)
block = asc * s + 0.5 * size + tsize
top = (H - block) / 2
wm = f'<g transform="translate({(W - wm_w) / 2 - pad * s:.2f} {top - pad * s:.2f}) scale({s:.5f})">{body}</g>'
td, _ = text_paths(FONTS[500], tag, tsize, (W - tend) / 2, top + asc * s + 0.5 * size + tsize * 0.75)
write(
    "kickstand-social.svg",
    f'<rect width="{W}" height="{H}" fill="{TILE}"/>{wm}<path d="{td}" fill="#A3A8B3"/>',
    W,
    H,
    "kickstand: Deploy, run, and debug homebrew with AI",
    note="Wordmark: gen_wordmark.py. Tagline: Space Grotesk (SIL OFL 1.1), as outlines.",
)
