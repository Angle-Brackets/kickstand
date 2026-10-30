# SPDX-License-Identifier: GPL-3.0-only
"""Renders the sysmodule's indicator: the swing icon (static pose) as 40x40 premultiplied RGBA bytes.

The display compositor blends premultiplied alpha, so straight alpha leaves a light rim where edge pixels
are nearly transparent. Rendered by cairo straight at size (no resampling ringing), then premultiplied. A
1 px transparent margin keeps the compositor's scaling filter (1080 down to the 720p panel) from pulling the
buffer padding past the layer's edge into the tile's edge.

Usage: python gen_tray.py kickstand-swing.svg ../../daemon/sysmodule/src/tray-icon.rgba preview.png
"""
import io
import sys

import cairosvg
from PIL import Image

SIZE = 40  # the indicator's layer size in vi's 1920x1080 space (indicator.rs)
MARGIN = 1

svg, out_bin, out_png = sys.argv[1:4]
inner = SIZE - 2 * MARGIN
png = cairosvg.svg2png(url=svg, output_width=inner, output_height=inner)
img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
img.paste(Image.open(io.BytesIO(png)).convert("RGBA"), (MARGIN, MARGIN))
img.save(out_png)
out = bytearray()
for r, g, b, a in img.get_flattened_data():
    out += bytes((r * a // 255, g * a // 255, b * a // 255, a))
open(out_bin, "wb").write(out)
