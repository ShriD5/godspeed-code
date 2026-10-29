"""Builds the landing page's icons, speedster sprite sheets and avatar thumbnails.

Needs Pillow with WebP. Run from the repo root:
    python3 site/scripts/images.py [--avatars DIR]
--avatars points at a folder of <handle>.jpg files (the launch-reply avatars).
"""
import argparse
import pathlib

from PIL import Image, ImageDraw

ROOT = pathlib.Path(__file__).resolve().parents[2]
OUT = ROOT / "site" / "public"
ICON = ROOT / "dist" / "godspeed.png"
SPRITES = ROOT / "crates" / "ui" / "assets" / "speedster"

# Frame order is the contract with site/public/app.js (FRAMES there).
FRAMES = [
    "jog_1", "jog_2", "jog_3", "jog_4",
    "sprint_1", "sprint_2", "sprint_3", "sprint_4",
    "sleeping_1", "sleeping_2",
    "idle_1", "idle_2",
    "alert_1", "alert_2",
    "celebrate_1", "celebrate_2",
]


def rounded(im: Image.Image, radius_frac: float = 0.225) -> Image.Image:
    size = im.size[0]
    scale = 4
    mask = Image.new("L", (size * scale, size * scale), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, size * scale - 1, size * scale - 1), radius=int(size * scale * radius_frac), fill=255
    )
    mask = mask.resize((size, size), Image.LANCZOS)
    out = im.convert("RGBA")
    out.putalpha(mask)
    return out


def icons() -> None:
    src = Image.open(ICON).convert("RGB")
    img = OUT / "img"
    for px in (64, 128, 256):
        rounded(src.resize((px, px), Image.LANCZOS)).save(img / f"icon-{px}.webp", quality=88, method=6)
    src.resize((180, 180), Image.LANCZOS).save(OUT / "apple-touch-icon.png", optimize=True)
    fav = rounded(src.resize((256, 256), Image.LANCZOS), 0.2)
    fav.save(OUT / "favicon.ico", sizes=[(16, 16), (32, 32), (48, 48)])
    fav.resize((32, 32), Image.LANCZOS).save(OUT / "favicon-32.png", optimize=True)


def sheet(source_size: int, px: int, name: str, quality: int) -> None:
    sheet = Image.new("RGBA", (px * len(FRAMES), px), (0, 0, 0, 0))
    for i, frame in enumerate(FRAMES):
        im = Image.open(SPRITES / f"{frame}@{source_size}.png").convert("RGBA").resize((px, px), Image.LANCZOS)
        sheet.paste(im, (i * px, 0), im)
    sheet.save(OUT / "img" / name, quality=quality, method=6)


def avatars(folder: pathlib.Path) -> None:
    dest = OUT / "img" / "people"
    dest.mkdir(parents=True, exist_ok=True)
    for jpg in sorted(folder.glob("*.jpg")):
        Image.open(jpg).convert("RGB").resize((96, 96), Image.LANCZOS).save(dest / f"{jpg.stem}.webp", quality=82, method=6)


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--avatars", type=pathlib.Path)
    args = ap.parse_args()
    (OUT / "img").mkdir(parents=True, exist_ok=True)
    icons()
    sheet(256, 88, "speedster-sm.webp", 80)
    sheet(512, 256, "speedster-lg.webp", 84)
    if args.avatars:
        avatars(args.avatars)
    for p in sorted(OUT.rglob("*")):
        if p.is_file() and p.suffix in {".webp", ".png", ".ico"}:
            print(f"{p.stat().st_size:>8}  {p.relative_to(OUT)}")
