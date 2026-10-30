# SPDX-License-Identifier: GPL-3.0-only
"""Renders the sysmodule's indicator: the swing icon (static pose) as 40x40 straight RGBA bytes.

Usage: python gen_tray.py kickstand-swing.svg ../../daemon/sysmodule/src/tray-icon.rgba preview.png
"""
import io, sys
import cairosvg
from PIL import Image
svg, out_bin, out_png = sys.argv[1:4]
SIZE = 40  # the indicator's layer size in vi's 1920x1080 space (indicator.rs)
png = cairosvg.svg2png(url=svg, output_width=160, output_height=160)
img = Image.open(io.BytesIO(png)).convert("RGBA").resize((SIZE, SIZE), Image.LANCZOS)
img.save(out_png)
open(out_bin, "wb").write(img.tobytes())
