#!/usr/bin/env python3
"""Render docs/PRIVACY.md into site/public/privacy.html (the /privacy page).

The markdown is the source of truth; rerun this after editing it and commit both:

    uv run --no-project --with markdown python site/scripts/privacy.py

Relative links (../edge/DEPLOY.md) are pointed at the file on GitHub.
"""
import html
import os
import posixpath
import re

import markdown

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SRC = os.path.join(ROOT, "docs/PRIVACY.md")
OUT = os.path.join(ROOT, "site/public/privacy.html")
REPO_BLOB = "https://github.com/ShriD5/godspeed-code/blob/main/"

with open(SRC, encoding="utf-8") as f:
    text = f.read()

body = markdown.markdown(text, extensions=["tables", "toc", "sane_lists"], output_format="html")


def fix_link(match: re.Match) -> str:
    href = html.unescape(match.group(1))
    if re.match(r"^(https?:|mailto:|#)", href):
        return match.group(0)
    path, _, frag = href.partition("#")
    resolved = posixpath.normpath(posixpath.join("docs", path))
    url = REPO_BLOB + resolved + (f"#{frag}" if frag else "")
    return f'href="{html.escape(url, quote=True)}"'


body = re.sub(r'href="([^"]*)"', fix_link, body)
# External links open normally; mark them so they don't leak the referrer.
body = body.replace('<a href="http', '<a rel="noopener" href="http')

page = f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<title>Privacy · Godspeed Code</title>
<meta name="description" content="What Godspeed Code's Mac and iPhone apps, cloud sync and beta waitlist collect, and how to turn it off or delete it.">
<meta name="theme-color" content="#0b0806">
<meta name="color-scheme" content="dark">
<link rel="canonical" href="https://godspeed-code.pages.dev/privacy">
<link rel="icon" href="/favicon.ico" sizes="48x48">
<link rel="icon" href="/favicon-32.png" type="image/png" sizes="32x32">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
<!-- Generated from docs/PRIVACY.md by site/scripts/privacy.py. Edit the markdown, not this file. -->
<style>
@font-face{{font-family:Geist;src:url(/fonts/geist.woff2) format("woff2");font-weight:100 900;font-display:swap}}
:root{{--bg:#0b0806;--panel:#15110d;--line:#2a221a;--ink:#efe7da;--muted:#a89d8d;--gold:#f2b63f}}
*{{box-sizing:border-box}}
body{{margin:0;background:var(--bg);color:var(--ink);font:16px/1.65 Geist,ui-sans-serif,system-ui,-apple-system,sans-serif;-webkit-font-smoothing:antialiased}}
.top{{max-width:820px;margin:0 auto;padding:22px 24px 0;display:flex;align-items:center;gap:12px}}
.top a{{color:var(--ink);text-decoration:none;display:flex;align-items:center;gap:10px;font-weight:600}}
.top img{{border-radius:8px}}
main{{max-width:820px;margin:0 auto;padding:12px 24px 72px}}
h1{{font-size:40px;letter-spacing:-.03em;margin:28px 0 8px}}
h2{{font-size:26px;letter-spacing:-.02em;margin:48px 0 10px;padding-top:12px;border-top:1px solid var(--line)}}
h3{{font-size:18px;margin:30px 0 8px}}
p,li{{color:#ddd3c4}}
a{{color:var(--gold)}}
code{{font:14px ui-monospace,Menlo,monospace;background:var(--panel);padding:1px 5px;border-radius:5px}}
table{{width:100%;border-collapse:collapse;margin:14px 0;font-size:14px;display:block;overflow-x:auto}}
th,td{{text-align:left;vertical-align:top;padding:8px 10px;border-bottom:1px solid var(--line)}}
th{{color:var(--muted);font-weight:600}}
footer{{max-width:820px;margin:0 auto;padding:0 24px 48px;color:var(--muted);font-size:14px}}
</style>
</head>
<body>
<header class="top"><a href="/"><img src="/favicon-32.png" width="28" height="28" alt="">Godspeed Code</a></header>
<main>
{body}
</main>
<footer><a href="/">Home</a> · <a href="https://github.com/ShriD5/godspeed-code/blob/main/docs/PRIVACY.md">Source of this page</a></footer>
</body>
</html>
"""

os.makedirs(os.path.dirname(OUT), exist_ok=True)
with open(OUT, "w", encoding="utf-8") as f:
    f.write(page)
print(f"wrote {os.path.relpath(OUT, ROOT)}")
