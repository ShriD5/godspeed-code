# Packaging

## Linux (implemented)

```sh
scripts/package-linux.sh            # release build (thin LTO, stripped)
PROFILE=debug scripts/package-linux.sh   # fast smoke package
```

Produces `target/package/godspeed-<version>-linux-<arch>.tar.gz` containing:

- `godspeed` — the binary (headed by default; `godspeed headless` runs the engine alone)
- `godspeed.desktop` — XDG desktop entry template (`Exec=godspeed` for packagers;
  the installers rewrite `Exec`, `TryExec`, and `Icon` to absolute paths under
  `~/.godspeed/app/current`, since `~/.local/bin` is often not on a desktop
  session's `PATH`)
- `godspeed.png` — 1024×1024 Godspeed app icon
- `install.sh` — installs into `~/.godspeed/app/<version>` behind a `current`
  symlink (the curl installer's layout, which the in-app updater manages),
  links `~/.local/bin/godspeed` to it, and writes the desktop entry and icon under
  `$XDG_DATA_HOME` (default `~/.local/share`). The curl installer does the same
  from the extracted tarball; `scripts/test-linux-desktop-entry.sh` checks both

The release profile in the root `Cargo.toml` sets `lto = "thin"` and
`strip = "symbols"` for distribution builds.

## macOS

```sh
scripts/package-macos.sh    # → target/package/godspeed-<version>-macos-<arch>.dmg
```

Builds the release binary, assembles `Godspeed.app` (Info.plist + icns), ad-hoc
signs it (set `CODESIGN_IDENTITY` for a real Developer ID), and wraps it in a
dmg. The auto-update tarball retains an internal `Godspeed.app` path so older
installed builds can update into Godspeed. CI runs this on tags
(`.github/workflows/release.yml`). The manual steps it automates, for reference
(run on a macOS host — gpui needs Metal; no cross-build from Linux):

1. Build the universal (or per-arch) binary:
   ```sh
   cargo build --release -p godspeed --target aarch64-apple-darwin
   cargo build --release -p godspeed --target x86_64-apple-darwin
   lipo -create -output godspeed \
     target/aarch64-apple-darwin/release/godspeed \
     target/x86_64-apple-darwin/release/godspeed
   ```
2. Assemble the bundle:
   ```sh
   mkdir -p Godspeed.app/Contents/{MacOS,Resources}
   cp godspeed Godspeed.app/Contents/MacOS/godspeed
   sed "s/__VERSION__/$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')/" \
     dist/macos/Info.plist > Godspeed.app/Contents/Info.plist
   ```
3. Icon: generate `godspeed.icns` from `dist/macos/icon-1024.png` (the macOS-shaped
   variant of the artwork — squircle mask, margins, and shadow pre-baked, since
   `sips` can't apply an alpha mask) and place it at
   `Godspeed.app/Contents/Resources/godspeed.icns`:
   ```sh
   mkdir godspeed.iconset && sips -z 256 256 dist/macos/icon-1024.png --out godspeed.iconset/icon_256x256.png
   iconutil -c icns godspeed.iconset -o Godspeed.app/Contents/Resources/godspeed.icns
   ```
4. Sign + notarize (required for distribution):
   ```sh
   codesign --deep --force --options runtime --sign "Developer ID Application: …" Godspeed.app
   xcrun notarytool submit Godspeed.zip --keychain-profile … --wait
   xcrun stapler staple Godspeed.app
   ```
5. Ship as a `.dmg` (`hdiutil create -volname Godspeed -srcfolder Godspeed.app -ov -format UDZO Godspeed.dmg`).

## Windows

```powershell
./scripts/package-windows.ps1 -ReleasesUrl https://github.com/ShriD5/godspeed-code/releases/latest/download
```

Produces, under `target/package/`:

- `godspeed-<version>-windows-<arch>-setup.exe` — the per-user installer built
  from `dist/windows/godspeed.iss` with Inno Setup 6
- `godspeed-<version>-windows-<arch>.zip` — the portable package
- `godspeed-<version>-windows-<arch>.exe` — the bare executable the in-app
  updater downloads

The installer and the zip both carry `zeron-update.json`, the marker that lets
the app update itself in place. CI runs `scripts/test-windows-installer.ps1`
against the setup on every Windows build.
