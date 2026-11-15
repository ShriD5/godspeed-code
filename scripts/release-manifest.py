#!/usr/bin/env python3
"""Write the in-app updater's release manifest.

    scripts/release-manifest.py [--version X.Y.Z] [--dir target/package]
                                [--out <dir>/manifest.json] [FILE ...]

The desktop updater (crates/update, `Manifest`) reads `manifest.json` from the
release feed, by default
https://github.com/ShriD5/godspeed-code/releases/latest/download/manifest.json,
so the file must be uploaded as an asset of every GitHub release, next to the
artifacts it lists. Shape:

    {
      "version": "0.5.8",
      "files": {
        "godspeed-0.5.8-macos-arm64-app.tar.gz": {"sha256": "<hex>", "size": 123},
        ...
      }
    }

Linux tarballs are not in `manifest.json`: the Linux release workflow
(.github/workflows/release-linux.yml) writes `manifest-linux.json` with this
script (`--out <dir>/manifest-linux.json`), which Linux installs read first.

Without FILE arguments every `godspeed-<version>-*` artifact in --dir is listed
(the dmg and the `-app.tar.gz` the updater installs). The version defaults to
`[workspace.package] version` in Cargo.toml; the release tag must be
`v<version>`, because the updater downloads artifacts from
`releases/download/v<version>/<file>`.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
VERSION_RE = re.compile(r"^\d+(\.\d+)*$")


def workspace_version() -> str:
    text = (ROOT / "Cargo.toml").read_text()
    match = re.search(r'^version\s*=\s*"([^"]+)"', text, re.MULTILINE)
    if not match:
        sys.exit("release-manifest: no version in Cargo.toml")
    return match.group(1)


def file_meta(path: Path) -> dict:
    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
            size += len(chunk)
    return {"sha256": digest.hexdigest(), "size": size}


def build_manifest(version: str, files: list[Path]) -> dict:
    if not VERSION_RE.match(version):
        sys.exit(f"release-manifest: version {version!r} is not dotted digits")
    entries = {}
    for path in sorted(files, key=lambda p: p.name):
        if not path.is_file():
            sys.exit(f"release-manifest: {path} is not a file")
        entries[path.name] = file_meta(path)
    if not entries:
        sys.exit("release-manifest: no artifacts to list")
    return {"version": version, "files": entries}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--version", default=None)
    parser.add_argument("--dir", type=Path, default=ROOT / "target" / "package")
    parser.add_argument("--out", type=Path, default=None)
    parser.add_argument("files", nargs="*", type=Path)
    args = parser.parse_args()

    version = args.version or workspace_version()
    files = args.files or sorted(args.dir.glob(f"godspeed-{version}-*"))
    files = [f for f in files if f.name != "manifest.json"]
    manifest = build_manifest(version, files)
    out = args.out or (args.dir / "manifest.json")
    # `manifest.json` is the macOS feed; per-platform feeds such as
    # `manifest-linux.json` (written by .github/workflows/release-linux.yml)
    # list only their own tarballs.
    app_tarballs = [name for name in manifest["files"] if name.endswith("-app.tar.gz")]
    if out.name == "manifest.json" and not app_tarballs:
        print(
            "release-manifest: warning: no -app.tar.gz listed; macOS apps can't update",
            file=sys.stderr,
        )
    out.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(f"release-manifest: wrote {out} ({version}, {len(manifest['files'])} files)")


if __name__ == "__main__":
    main()
