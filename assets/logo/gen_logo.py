# SPDX-License-Identifier: GPL-3.0-only
"""Generates Kickstand's README/social SVGs with Space Grotesk text converted to paths.

Usage: python gen_logo.py <SpaceGrotesk[wght].ttf> <out dir>  (needs fonttools; the font is google/fonts ofl/spacegrotesk)
"""
import math
import sys

from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont
from fontTools.varLib import instancer

FONT_PATH, OUT = sys.argv[1], sys.argv[2]

INK, PAPER, RED, TILE = "#17191E", "#F2F0EA", "#FF4554", "#15171C"
GH_LIGHT_BG, GH_DARK_BG = "#FFFFFF", "#0D1117"


def load(weight):
    return instancer.instantiateVariableFont(TTFont(FONT_PATH), {"wght": weight})


FONTS = {600: load(600), 500: load(500)}


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


def metrics(font, size):
    scale = size / font["head"].unitsPerEm
    gs = font.getGlyphSet()
    from fontTools.pens.boundsPen import BoundsPen

    def bounds(ch):
        bp = BoundsPen(gs)
        gs[font.getBestCmap()[ord(ch)]].draw(bp)
        return [v * scale for v in bp.bounds]

    x_height = bounds("x")[3]
    ascender = bounds("d")[3]
    i_b = bounds("l")
    stem = i_b[2] - i_b[0]
    return x_height, ascender, stem


def k_mark(x0, baseline, size, color, pin, animate):
    """The wordmark's k, sized like the font's own k: a stem, an arm to the x-height and the kickstand leg
    to the baseline. Returns the svg and where the next letter starts."""
    font = FONTS[600]
    xh, asc, w = metrics(font, size)
    scale = size / font["head"].unitsPerEm
    from fontTools.pens.boundsPen import BoundsPen

    gs = font.getGlyphSet()
    kname = font.getBestCmap()[ord("k")]
    bp = BoundsPen(gs)
    gs[kname].draw(bp)
    kxmin, _, kxmax, _ = (v * scale for v in bp.bounds)
    adv = font["hmtx"][kname][0] * scale
    # Narrower than the font's k (whose leg starts on the arm): the kickstand pivots on the stem.
    right = x0 + w + 0.6 * xh
    r = w * 0.22
    jx, jy = x0 + w * 0.75, baseline - 0.5 * xh

    def bar(tx, ty):
        vx, vy = tx - jx, ty - jy
        return math.hypot(vx, vy), math.degrees(math.atan2(-vx, vy))

    arm, arm_deg = bar(right - w * 0.62, baseline - xh + w * 0.42)
    leg, leg_deg = bar(right - w * 0.62, baseline - w * 0.42)
    arm += w * 0.5
    leg += w * 0.5
    if animate:
        vals = ";".join(f"{leg_deg + d:.1f}" for d in (0, 0, -16, 4, -1, 0, 0))
        anim = (f'<animateTransform attributeName="transform" type="rotate" values="{vals}" '
                'keyTimes="0;0.55;0.64;0.72;0.78;0.84;1" dur="3.6s" repeatCount="indefinite"/>')
    else:
        anim = ""
    svg = (
        f'<rect x="{x0:.2f}" y="{baseline - asc:.2f}" width="{w:.2f}" height="{asc:.2f}" rx="{r:.2f}" fill="{color}"/>'
        f'<g transform="translate({jx:.2f} {jy:.2f})">'
        f'<rect x="{-w / 2:.2f}" y="0" width="{w:.2f}" height="{arm:.2f}" rx="{r:.2f}" fill="{color}" transform="rotate({arm_deg:.2f})"/>'
        f'<g transform="rotate({leg_deg:.2f})">{anim}'
        f'<rect x="{-w / 2:.2f}" y="0" width="{w:.2f}" height="{leg:.2f}" rx="{r:.2f}" fill="{RED}"/>'
        f'<rect x="{-w / 2:.2f}" y="{leg - w * 0.45:.2f}" width="{w:.2f}" height="{w * 0.45:.2f}" rx="{r * 0.8:.2f}" fill="#000000" fill-opacity="0.25"/>'
        f'<circle cx="0" cy="{w * 0.42:.2f}" r="{w * 0.15:.2f}" fill="{pin}"/>'
        "</g></g>"
    )
    return svg, right + size * 0.035


def wordmark(size, x, baseline, color, pin, animate=False):
    k, right = k_mark(x, baseline, size, color, pin, animate)
    d, end = text_paths(FONTS[600], "ickstand", size, right, baseline, tracking=-size * 0.023)
    return k + f'<path d="{d}" fill="{color}"/>', end


def write(name, body, w, h, label):
    with open(f"{OUT}/{name}", "w") as f:
        f.write(
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0f}" height="{h:.0f}" viewBox="0 0 {w:.2f} {h:.2f}" '
            f'role="img" aria-label="{label}">\n'
            "<!-- SPDX-License-Identifier: GPL-3.0-only. Letters: Space Grotesk (SIL OFL 1.1), as outlines. -->\n"
            f"{body}\n</svg>\n"
        )


# Wordmark for the README, light and dark themes (transparent background).
SIZE, PAD = 128, 8
_, asc, _ = metrics(FONTS[600], SIZE)
base = PAD + asc
for theme, color, pin in (("light", INK, GH_LIGHT_BG), ("dark", PAPER, GH_DARK_BG)):
    body, end = wordmark(SIZE, PAD, base, color, pin)
    write(f"kickstand-wordmark-{theme}.svg", body, end + PAD, base + PAD, "kickstand")
print(f"wordmark: {end + PAD:.1f} x {base + PAD:.1f}")

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

# Social preview, 1280x640: wordmark and tagline centred on the dark tile colour.
W, H = 1280, 640
size = 168
probe, pend = wordmark(size, 0, 0, PAPER, TILE)
x = (W - pend) / 2
_, asc, _ = metrics(FONTS[600], size)
tag = "Deploy, run, and debug homebrew with AI"
tsize = 44
_, tend = text_paths(FONTS[500], tag, tsize, 0, 0)
block = asc + 0.5 * size + tsize
top = (H - block) / 2
wm, _ = wordmark(size, x, top + asc, PAPER, TILE)
td, _ = text_paths(FONTS[500], tag, tsize, (W - tend) / 2, top + asc + 0.5 * size + tsize * 0.75)
write(
    "kickstand-social.svg",
    f'<rect width="{W}" height="{H}" fill="{TILE}"/>{wm}<path d="{td}" fill="#A3A8B3"/>',
    W,
    H,
    "kickstand: Deploy, run, and debug homebrew with AI",
)
