# SPDX-License-Identifier: GPL-3.0-only
"""Kickstand's wordmark: "kickstand" with letters built like the mark's k.

Every letter is a monoline stroke of the k's stem width, cut square at the ends and rounded with the k's
small corner radius, so "ickstand" reads as the same construction as the k instead of a typeface.
Metrics (x-height, ascender, stem) are Space Grotesk 600's at 128 px, which the first wordmark was set in.

Usage: python gen_wordmark.py <out dir>  (needs shapely). Writes kickstand-wordmark-{light,dark}.svg;
gen_logo.py uses build() for the social preview.
"""
import math
import sys

from shapely.geometry import LineString, Polygon, box
from shapely.ops import unary_union
from shapely import affinity

SIZE = 128
W = 14.65            # stem width (the k's)
R = 3.22             # corner radius (the k's rx)
ASC = 89.60          # ascender height
XH = 63.26           # x-height
PAD = 8.0
BASE = PAD + ASC     # baseline y
TOP = BASE - XH      # x-height y
O = 1.2              # overshoot for round letters
INK, PAPER, RED = "#17191E", "#F2F0EA", "#FF4554"


def rounded(geom, r=R):
    """Rounds every convex corner with radius r (the k's rectangles have rx = r)."""
    return geom.buffer(-r, join_style="round", quad_segs=16).buffer(r, join_style="round", quad_segs=16)


def stroke(points):
    return LineString(points).buffer(W / 2, cap_style="flat", join_style="round", quad_segs=24)


def rect(x0, y0, x1, y1):
    return box(x0, y0, x1, y1)


def arc(cx, cy, rx, ry, a0, a1, n=96):
    """Points on an ellipse (centreline), angles in degrees, y up like math (so screen y is flipped)."""
    pts = []
    for i in range(n + 1):
        a = math.radians(a0 + (a1 - a0) * i / n)
        pts.append((cx + rx * math.cos(a), cy - ry * math.sin(a)))
    return pts


def ring(cx, cy, orx, ory):
    """A closed bowl: outer ellipse radii orx/ory, wall W."""
    outer = affinity.scale(Polygon(arc(0, 0, 1, 1, 0, 360, 192)), orx, ory, origin=(0, 0))
    inner = affinity.scale(Polygon(arc(0, 0, 1, 1, 0, 360, 192)), orx - W, ory - W, origin=(0, 0))
    return affinity.translate(outer.difference(inner), cx, cy)


# ---- letters: each returns (geometry, advance width) drawn from x = 0 ----

BOWL_W = 57.0                      # outer width of c/a/d bowls
BOWL_RX, BOWL_RY = BOWL_W / 2, (XH + 2 * O) / 2
BOWL_CY = BASE - XH / 2


def g_i():
    stem = rounded(rect(0, TOP, W, BASE))
    dot = rounded(rect(0, PAD, W, PAD + W))
    return unary_union([stem, dot])


def g_c():
    crx, cry = BOWL_RX - W / 2, BOWL_RY - W / 2
    cx = BOWL_RX
    return rounded(stroke(arc(cx, BOWL_CY, crx, cry, 42, 318)))


def k_parts(x0, color=INK):
    """The mark's k geometry (same maths as gen_logo.k_mark), as rectangles."""
    right = x0 + W + 0.6 * XH
    jx, jy = x0 + W * 0.75, BASE - 0.5 * XH

    def bar(tx, ty):
        vx, vy = tx - jx, ty - jy
        return math.hypot(vx, vy), math.degrees(math.atan2(-vx, vy))

    arm, arm_deg = bar(right - W * 0.62, BASE - XH + W * 0.42)
    leg, leg_deg = bar(right - W * 0.62, BASE - W * 0.42)
    return right, jx, jy, arm + W * 0.5, arm_deg, leg + W * 0.5, leg_deg


def g_k():
    right, jx, jy, arm, arm_deg, leg, leg_deg = k_parts(0)
    stem = rounded(rect(0, BASE - ASC, W, BASE))
    bars = []
    for length, deg in ((arm, arm_deg), (leg, leg_deg)):
        b = rounded(rect(-W / 2, 0, W / 2, length))
        b = affinity.rotate(b, deg, origin=(0, 0))
        bars.append(affinity.translate(b, jx, jy))
    return unary_union([stem] + bars)


def g_s():
    sw = 50.0                                    # outer width
    meet = BOWL_CY - 1.6                         # spine crosses a little above the middle
    t_top = TOP - O + W / 2
    b_bot = BASE + O - W / 2
    ry1 = (meet - t_top) / 2
    ry2 = (b_bot - meet) / 2
    rx1 = sw / 2 - W / 2 - 1.5                   # top bowl a touch narrower
    rx2 = sw / 2 - W / 2
    cx = sw / 2
    top = arc(cx, t_top + ry1, rx1, ry1, 28, 270, 120)
    bot = arc(cx, b_bot - ry2, rx2, ry2, 90, -152, 120)
    return rounded(stroke(top + bot[1:]))


def g_t():
    sx = 10.0                                    # stem left edge
    stem = rounded(rect(sx, PAD + 10, sx + W, BASE))
    # The bar reaches 12 past the stem on the right (10 on the left): longer, it held the a away and left
    # a gap under it.
    bar = rounded(rect(0, TOP, sx + W + 12, TOP + W))
    return unary_union([stem, bar])


def bowl_with_stem(stem_top):
    bowl = ring(BOWL_RX, BOWL_CY, BOWL_RX, BOWL_RY)
    sx = BOWL_W - W
    stem = rounded(rect(sx, stem_top, BOWL_W, BASE))
    # cut the bowl's right wall where the stem stands so the join is clean
    return unary_union([bowl.difference(rect(sx + W / 2, -1e3, 1e3, 1e3)), stem])


def g_a():
    return bowl_with_stem(TOP)


def g_d():
    return bowl_with_stem(BASE - ASC)


def g_n():
    nw = 52.0                                    # outer width
    xl, xr = W / 2, nw - W / 2
    rx = (xr - xl) / 2
    ry = rx * 1.02
    cy = TOP - O + W / 2 + ry
    arch = stroke(arc((xl + xr) / 2, cy, rx, ry, 180, 0) + [(xr, BASE + W)])
    # union the raw shapes, cut the feet flat at the baseline, then round once: no notch at the join
    raw = unary_union([rect(0, TOP, W, BASE), arch]).intersection(rect(-1e3, -1e3, 1e3, BASE))
    return rounded(raw)


GLYPHS = {"i": g_i, "c": g_c, "k": g_k, "s": g_s, "t": g_t, "a": g_a, "n": g_n, "d": g_d}

# Optical spacing: for each pair, the white area between the letters across the x-height band (each row's
# gap capped at CAP, so open shapes like c, k and t don't drift away) is made equal to that of two stems
# TARGET apart. MIN keeps diagonals and terminals from getting closer than that anywhere.
TARGET = 17.0
CAP = 25.0
MIN = 8.0
ROWS = 64
# By eye, after the optical spacing: the k's open side makes k-s look wider than it measures.
KERN = {"ks": -1.5}


def profile(g, side):
    """Per row of the x-height band: the ink's rightmost (side=1) or leftmost (side=-1) x, or None."""
    out = []
    for i in range(ROWS):
        y = TOP + (i + 0.5) * XH / ROWS
        strip = g.intersection(rect(-1e4, y - 0.4, 1e4, y + 0.4))
        if strip.is_empty:
            out.append(None)
        else:
            x0, _, x1, _ = strip.bounds
            out.append(x1 if side == 1 else x0)
    return out


def place(left_prof, right_prof):
    """Offset to add to the right glyph's x so the pair's capped white area matches TARGET."""
    def mean_gap(dx):
        gaps = []
        for l, r in zip(left_prof, right_prof):
            if l is None or r is None:
                gaps.append(CAP)
            else:
                gaps.append(min(CAP, r + dx - l))
        return sum(gaps) / len(gaps)

    def min_gap(dx):
        return min(r + dx - l for l, r in zip(left_prof, right_prof) if l is not None and r is not None)

    lo, hi = -500.0, 500.0
    for _ in range(60):
        mid = (lo + hi) / 2
        if mean_gap(mid) < TARGET:
            lo = mid
        else:
            hi = mid
    dx = hi
    if min_gap(dx) < MIN:
        dx += MIN - min_gap(dx)
    return dx


def mark_k_geom(x0):
    right, jx, jy, arm, arm_deg, leg, leg_deg = k_parts(x0)
    parts = [rect(x0, BASE - ASC, x0 + W, BASE)]
    for length, deg in ((arm, arm_deg), (leg, leg_deg)):
        b = affinity.rotate(rect(-W / 2, 0, W / 2, length), deg, origin=(0, 0))
        parts.append(affinity.translate(b, jx, jy))
    return unary_union(parts)


def path_d(geom):
    polys = getattr(geom, "geoms", [geom])
    out = []
    for p in polys:
        for ring_ in [p.exterior, *p.interiors]:
            pts = list(ring_.coords)[:-1]
            out.append("M" + " L".join(f"{x:.2f} {y:.2f}" for x, y in pts) + "Z")
    return " ".join(out)


def build(pin, color, animate=False):
    # The red-legged k, exactly as gen_logo.py draws it.
    x0 = PAD
    right, jx, jy, arm, arm_deg, leg, leg_deg = k_parts(x0)
    r = W * 0.22
    anim = ""
    if animate:
        vals = ";".join(f"{leg_deg + d:.1f}" for d in (0, 0, -16, 4, -1, 0, 0))
        anim = (f'<animateTransform attributeName="transform" type="rotate" values="{vals}" '
                'keyTimes="0;0.55;0.64;0.72;0.78;0.84;1" dur="3.6s" repeatCount="indefinite"/>')
    k = (
        f'<rect x="{x0:.2f}" y="{BASE - ASC:.2f}" width="{W:.2f}" height="{ASC:.2f}" rx="{r:.2f}" fill="{color}"/>'
        f'<g transform="translate({jx:.2f} {jy:.2f})">'
        f'<rect x="{-W / 2:.2f}" y="0" width="{W:.2f}" height="{arm:.2f}" rx="{r:.2f}" fill="{color}" transform="rotate({arm_deg:.2f})"/>'
        f'<g transform="rotate({leg_deg:.2f})">{anim}'
        f'<rect x="{-W / 2:.2f}" y="0" width="{W:.2f}" height="{leg:.2f}" rx="{r:.2f}" fill="{RED}"/>'
        f'<rect x="{-W / 2:.2f}" y="{leg - W * 0.45:.2f}" width="{W:.2f}" height="{W * 0.45:.2f}" rx="{r * 0.8:.2f}" fill="#000000" fill-opacity="0.25"/>'
        f'<circle cx="0" cy="{W * 0.42:.2f}" r="{W * 0.15:.2f}" fill="{pin}"/>'
        "</g></g>"
    )
    prev = mark_k_geom(x0)
    prev_prof = profile(prev, 1)
    shapes = []
    prev_ch = "k"
    for ch in "ickstand":
        g = GLYPHS[ch]()
        dx = place(prev_prof, profile(g, -1)) + KERN.get(prev_ch + ch, 0.0)
        prev_ch = ch
        g = affinity.translate(g, dx, 0)
        shapes.append(g)
        prev_prof = profile(g, 1)
    end = shapes[-1].bounds[2] + PAD
    return k + f'<path d="{path_d(unary_union(shapes))}" fill="{color}" fill-rule="evenodd"/>', end


def write(path, color, pin, animate=False):
    body, w = build(pin, color, animate)
    h = BASE + PAD
    with open(path, "w") as f:
        f.write(f'<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0f}" height="{h:.0f}" viewBox="0 0 {w:.2f} {h:.2f}" '
                f'role="img" aria-label="kickstand">\n<!-- SPDX-License-Identifier: GPL-3.0-only. Drawn by gen_wordmark.py. -->\n'
                f'{body}\n</svg>\n')
    print(path, f"{w:.1f} x {h:.1f}")


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else "."
    # The pin shows the page behind it: GitHub's light and dark backgrounds.
    write(f"{out}/kickstand-wordmark-light.svg", INK, "#FFFFFF")
    write(f"{out}/kickstand-wordmark-dark.svg", PAPER, "#0D1117")
