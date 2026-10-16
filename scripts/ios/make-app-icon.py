#!/usr/bin/env python3
"""Build the iOS app icon (1024x1024, opaque, full-bleed) from dist/godspeed.png.

dist/godspeed.png is the desktop artwork cropped to its rounded square, so its
four corners are black. iOS applies its own mask and wants a full-bleed square,
so the corners are filled with a blurred, slightly zoomed copy of the artwork
(the iOS mask hides them anyway; this just keeps any other rendering clean).

    uv run --no-project --with pillow python scripts/ios/make-app-icon.py
"""
import os

from PIL import Image, ImageDraw, ImageFilter

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SRC = os.path.join(ROOT, "dist/godspeed.png")
OUT = os.path.join(ROOT, "apps/ios/Godspeed/Assets.xcassets/AppIcon.appiconset/AppIcon1024.png")
SIZE = 1024
CORNER_RADIUS = 112  # just past the source's black corners (~96 px)

art = Image.open(SRC).convert("RGB").resize((SIZE, SIZE), Image.LANCZOS)

zoom = int(SIZE * 1.3)
offset = (zoom - SIZE) // 2
fill = art.resize((zoom, zoom), Image.LANCZOS).crop((offset, offset, offset + SIZE, offset + SIZE))
fill = fill.filter(ImageFilter.GaussianBlur(24))

mask = Image.new("L", (SIZE, SIZE), 0)
ImageDraw.Draw(mask).rounded_rectangle((0, 0, SIZE - 1, SIZE - 1), radius=CORNER_RADIUS, fill=255)
mask = mask.filter(ImageFilter.GaussianBlur(2))

icon = Image.composite(art, fill, mask)
icon.save(OUT, "PNG", optimize=True)
print(f"wrote {os.path.relpath(OUT, ROOT)}")
