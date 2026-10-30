# SPDX-License-Identifier: GPL-3.0-only
"""Renders the mark for the NRO's status screen header: SIZE x SIZE straight-alpha RGBA bytes (the screen
blends it onto its own background).

Usage: python gen_nro_mark.py kickstand-mark-512.png ../../daemon/app/src/mark.rgba preview.png
"""
import sys

from PIL import Image

SIZE = 64  # screen.rs MARK_SIZE

src, out_bin, out_png = sys.argv[1:4]
img = Image.open(src).convert("RGBA").resize((SIZE, SIZE), Image.LANCZOS)
img.save(out_png)
open(out_bin, "wb").write(img.tobytes())
