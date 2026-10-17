import sys, os
from PIL import Image
raw, out = sys.argv[1], sys.argv[2]
names = ["idle_1","idle_2","sleeping_1","sleeping_2","jog_1","jog_2","jog_3","jog_4",
         "sprint_1","sprint_2","sprint_3","sprint_4","alert_1","alert_2","celebrate_1","celebrate_2","tired","oops"]
frames = {}
for n in names:
    im = Image.open(f"{raw}/{n}.png").convert("RGBA")
    a = im.getchannel("A").point(lambda v: 0 if v < 16 else v)   # drop generator haze
    im.putalpha(a)
    bb = a.point(lambda v: 255 if v > 40 else 0).getbbox()
    frames[n] = im.crop(bb)
# one scale for every frame so the character never changes size between frames
C, INNER, FOOT = 1024, 900, 60
s = INNER / max(max(f.size) for f in frames.values())
os.makedirs(out, exist_ok=True)
total = 0
for n, f in frames.items():
    f = f.resize((round(f.width*s), round(f.height*s)), Image.LANCZOS)
    canvas = Image.new("RGBA", (C, C), (0,0,0,0))
    canvas.alpha_composite(f, ((C - f.width)//2, C - FOOT - f.height))   # shared baseline
    for size, quant in ((512, True), (256, True), (44, False), (22, False)):
        im = canvas.resize((size, size), Image.LANCZOS)
        if quant:
            im = im.quantize(colors=256, method=Image.Quantize.FASTOCTREE, dither=Image.Dither.NONE)
        p = f"{out}/{n}@{size}.png"
        im.save(p, optimize=True); total += os.path.getsize(p)
print(f"scale {s:.3f}, {len(frames)*4} files, {total/1024:.0f} KiB")
