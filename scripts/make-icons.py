#!/usr/bin/env python3
"""Regenerate every app icon from the full-bleed artwork in dist/godspeed.png.

- dist/macos/icon-1024.png: macOS squircle (824 px body, 185.4 px corners)
  with the standard margin and a soft drop shadow baked in, because sips
  can't alpha-mask when scripts/package-macos.sh builds the .icns.
- dist/windows/godspeed.ico: 16-256 px.
- The iOS app icon, via scripts/ios/make-app-icon.py.

Usage: python3 scripts/make-icons.py [artwork.png]
(with an argument, the artwork is first copied to dist/godspeed.png).
"""

import os
import shutil
import subprocess
import sys

from PIL import Image, ImageDraw, ImageFilter

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ART = os.path.join(ROOT, "dist/godspeed.png")
SIZE, BODY, RADIUS = 1024, 824, 185.4


def squircle_mask(size, radius, scale=4):
    big = Image.new("L", (size * scale, size * scale), 0)
    ImageDraw.Draw(big).rounded_rectangle(
        [0, 0, size * scale - 1, size * scale - 1], radius * scale, fill=255
    )
    return big.resize((size, size), Image.LANCZOS)


def macos_icon(art):
    off = (SIZE - BODY) // 2
    mask = squircle_mask(BODY, RADIUS)
    shadow = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    shadow_alpha = Image.new("L", (SIZE, SIZE), 0)
    shadow_alpha.paste(mask.point(lambda v: v * 0.35), (off, off + 12))
    shadow.putalpha(shadow_alpha.filter(ImageFilter.GaussianBlur(20)))
    icon = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    icon.alpha_composite(shadow)
    body = art.convert("RGBA").resize((BODY, BODY), Image.LANCZOS)
    icon.paste(body, (off, off), mask)
    return icon


def main():
    if len(sys.argv) > 1:
        shutil.copyfile(sys.argv[1], ART)
    art = Image.open(ART).convert("RGB")
    if art.size != (SIZE, SIZE):
        art = art.resize((SIZE, SIZE), Image.LANCZOS)
        art.save(ART)
    icon = macos_icon(art)
    icon.save(os.path.join(ROOT, "dist/macos/icon-1024.png"))
    icon.save(
        os.path.join(ROOT, "dist/windows/godspeed.ico"),
        sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)],
    )
    subprocess.run(
        [sys.executable, os.path.join(ROOT, "scripts/ios/make-app-icon.py")], check=True
    )
    print("icons regenerated from", ART)


if __name__ == "__main__":
    main()
