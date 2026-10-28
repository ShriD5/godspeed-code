# Godspeed handoff

Written 2026-10-15, updated 2026-10-16 05:30 IST. Read this first, then `ROADMAP.md`, `PARITY-T3.md` and `ISSUES-TRIAGE.md` in
this folder.

## What this is

Godspeed is one native app for every AI coding agent: Rust + GPUI, forked from
[Zeron](https://github.com/zeronsh/zeron) (MIT), aiming for T3 Code parity and better. The owner
wants it to be the fastest and best-looking option, and eventually open source.

- Repo: `~/Developer/godspeed`, PUBLIC (since 2026-10-19) at github.com/ShriD5/godspeed-code (`origin`).
  `upstream` is zeronsh/zeron, for cherry-picking fixes only. GitHub Actions runs every workflow
  on PRs and pushes to main (macOS and Windows included); minutes are free for public repos.
- `main` is at `3c65438c` and pushed. Parity with T3 was 54% at the last audit (before delegation, lifecycle and the leftovers landed).
- Installed build: `/Applications/Godspeed.app` is the release of `d752f8cd` (everything except the
  benchmarks doc), ad-hoc signed; `godspeed` CLI symlinked in `~/.local/bin`.
- The owner's real Godspeed data (`~/.godspeed`, 874 MB) already holds 369 imported chats: 100 from
  T3 (including the threads that built Godspeed), 35 Claude, 212 Codex, 22 Cursor. Claude and Codex
  chats continue their original sessions.

## Owner rules (follow these)

- Commits: conventional prefixes (`feat:`, `fix:`, `docs:` …), **no AI attribution, no
  Co-Authored-By lines**. Push only to `origin`.
- Never launch the GUI app or steal focus; no synthetic clicks/keys/screenshots. Builds and headless
  tests only; the owner checks UI by eye.
- Ask before anything public or outward-facing (making the repo public, posting).
- Never touch the owner's `~/.claude`, `~/.codex`, `~/.t3`, `~/.godspeed` data in tests.

## Build setup (important)

- The main disk is nearly full. All build output lives on the external drive:
  `.cargo/config.toml` (git-excluded) sets `target-dir = /Volumes/Lexar/build/godspeed-target`, and
  `~/Developer/godspeed/target` is a symlink to it (needed by `scripts/package-macos.sh`).
- `/Volumes/Lexar` is APFS. Give each parallel agent its own cache by cloning, which takes ~30 s and
  no extra space: `cp -c -R godspeed-target godspeed-target-<name>`. Never `cp -R` (52 GB).
- **Keep at most 2 parallel builds with `CARGO_BUILD_JOBS=4`.** The drive unmounted once under three
  heavy builds; it was recovered with `diskutil verifyVolume disk7s1` then `diskutil mount disk7s1`.
  Xcode also lives on that drive.
- Clean build ~22 min, incremental ~30 s–3 min, release ~13 min.
- Package and install: `PATH="$HOME/Library/Python/3.14/bin:$PATH" scripts/package-macos.sh`, then
  `rm -rf /Applications/Godspeed.app && ditto target/package/Godspeed.app /Applications/Godspeed.app`.
- Worktree agents must be launched with the shell's cwd inside `~/Developer/godspeed`.

## Tests

`cargo build -p godspeed`, `cargo check --workspace --tests`, then per crate:
`cargo test -p zeron-engine --lib` (546), `--test e2e` (29), `--test always_on_agents` (9),
`--test git_actions`, `--test chat_import`; `cargo test -p zeron-harness --lib` (372),
`--test claude` (27), `--test codex` (37), `--test opencode`, `--test acp`, `--test muse_live`
(needs `muse` installed; uses the free echo provider); `cargo test -p zeron-ui --lib --
--test-threads=1` (1792; the UI test binary can SIGABRT when run fully in parallel).

Known flaky only under heavy load (pass alone): `version_probe_bounds_hangs_and_rejects_nonzero`,
`runtime_survival::a_stop_right_after_send_stops_that_turn`.

## Done and merged

- Rebrand: user-facing names only; internal crate names stay `zeron-*` to keep upstream
  cherry-picks cheap. Local-only (edge is a `.invalid` placeholder, no baked WorkOS client).
- Muse Code harness (`crates/harness/src/muse`).
- Access modes (supervised / auto-accept edits [default] / auto / full) and approvals for every
  harness; Cursor is auto/full only and Pi is full only. Cycle with Cmd/Ctrl+Shift+M.
- Git write actions with generated commit/PR text (Changes pane, palette); "remove agent credits"
  setting, on by default.
- Always-on agents (`crates/engine/src/agents*`, Settings → Agents): cron, interval, one-shot,
  webhook (`POST /hooks/agents/{id}/{token}` on the IPC port), file watch; memory file; inbox.
- Chat import from T3, Claude Code, Codex and Cursor (`crates/engine/src/chat_import`, Settings →
  Import, `godspeed import`). Claude and Codex chats resume the same session; Cursor imports as
  history only. The owner has not run the real import yet.
- ~45 upstream issue fixes across drivers, engine and UI (see `ISSUES-TRIAGE.md`), plus a skill
  discovery fix (project skills first).
- App icon: painterly low-poly speedster with a single-fin helmet (`design/icon/godspeed-icon-3.png`, made by Codex in Zeron's style; no DC tells).

## Done on 2026-10-16

- Delegation that wakes the parent (`create_chat` / `create_chats` `notify`, durable outbox).
- Resume at reset hardened: crash-safe continuation, bound to the account, cancelled by Stop/archive.
- Thread lifecycle: settle, snooze (presets + typed times), wake, mark unread, auto-settle (off by
  default), undo with mod+z, "Working · Nm" and +/- diff stats on rows, Snoozed/Settled shelves.
- Leftovers: branch prefix setting, "also delete its worktree" on chat delete, unused worktrees list,
  "Waiting on N tasks" row label, shortcuts mod+[ / mod+] (back/forward), mod+shift+t (reopen tab),
  mod+shift+g (changes pane), mod+shift+d (settle).
- Headless benchmarks vs T3 in `BENCHMARKS.md`: start 133 ms vs 877 ms, idle footprint 12 MiB vs
  166 MiB, list 200 threads 0.8 ms vs 23 ms. Scripts and raw data in
  `/Volumes/Lexar/build/godspeed-coordination/bench/`.
- Work was split between a Codex session and Claude; the shared log is
  `/Volumes/Lexar/build/godspeed-coordination/progress.md`.

## Done on 2026-10-17

- Phase 0 of `PLAN.md`: the headless `--print` driver wastes no tokens; any extra limit use is
  server-side weighting of the `sdk-cli` entrypoint. Don't spoof the entrypoint.
- Phase 1: Needs-you inbox (attention service, risk ranking, keyboard queue, question timeout,
  Telegram approval digests). Merged `befc3b90`.
- Phase 2: worktree environments (`godspeed.json` `env`: copy, ports, setup/teardown, wake, branch
  template; sleep/wake). Merged `67a664c7`; template values are shell-quoted (`cdd6da2b`).
  Multi-repo workspaces are not started. Docs: `WORKTREES.md`.
- Speedster mascot frames (Codex-generated, 18 frames, 8 states) in `crates/ui/assets/speedster/`;
  plan in `design/speedster/PLAN.md`, export script in `design/speedster/tools/`. Not wired up yet.
- Always-on agents in the owner's install: Zeron sync (15:00 IST) and T3 watch (17:00 IST, paused
  during heavy builds); ledgers `UPSTREAM.md` and `T3-WATCH.md`.

## Next, in order

1. Owner's eyeball pass on 0.5.0 (checklist §10, §12b, §15b, §15c).
2. Fun, zero-token features left: lightning crackle on finished rows, menu-bar speedster.
3. `PLAN.md` phase 3 (close the laptop: Linux artifacts, "Run on a server", reconnect hardening;
   needs a VPS), phase 5 (PR
   autopilot; Codex's brief at `/Volumes/Lexar/build/godspeed-coordination/pr-actions-brief.md`).
4. Known flaky under load: `version_probe_bounds_hangs_and_rejects_nonzero`,
   `file_changes_fire_one_debounced_run`, two `fs_watch` engine lib tests, `focused_waiter…`.

Eyeball list: approval card; access-mode chip; Snooze submenu and typed-time field; Snoozed/Settled
shelves; "Working · Nm" and diff counts; Settings → General (auto-settle, worktree branch prefix,
unused worktrees); delete-chat worktree checkbox; Cmd+F find bar; palette message search; the usage
limit banner; Settings → Agents and Import.

Known small gaps: snoozed chats can still push notifications; one imported Claude "chat" is a
title-generation run (archive it). (The phone now has the shelves, Limited, the inbox, Mission
Control, Usage + widgets and receipts: branch `feat/ios-next`, see `apps/ios/README.md`.)

## Decisions already made

Cut: plan mode, account fallback, mid-thread provider switching (breaks prompt caching).
Maybe later: checkpoints, rewind, edit-from-here. Telegram for always-on agents shipped open-dots
style in 0.2.0. Skipped:
T3 Connect relay and mobile compatibility, SnapShots, cookie import. (The usage/cost page shipped in 0.5.1.)

Open questions for the owner: Antigravity only unlocks some models when the client claims to be
Zed (recommendation: don't spoof); the feature list in `ISSUES-TRIAGE.md` "Later".

## Releases

In-app updates (OTA) read GitHub Releases: the app fetches
`https://github.com/ShriD5/godspeed-code/releases/latest/download/manifest.json` on launch and
every 6 h (Settings → General → Updates, "Check for updates automatically", on by default; "Check
now" there and in the app menu), downloads `godspeed-<v>-macos-arm64-app.tar.gz` from the
`v<v>` release, verifies sha256 + size, `codesign --verify --deep --strict` with a Developer ID
requirement for the running app's Team ID (QPUV967B5U), `spctl` notarization, and the bundle's
`--version`, then offers "Godspeed <v> is ready — Restart" in the sidebar (installs on quit
otherwise). Never downgrades. `GODSPEED_RELEASES_URL=https://…` overrides the feed for testing.
Apps up to 0.5.6 still poll `{edge}/releases/*`; the edge (once redeployed) answers those with
a 302 to the same GitHub `latest/download` asset, so they can update too (their older updater
checks sha256 + `--version` only). Until the edge is redeployed they need one manual install.

Every release, in order:

1. Bump `[workspace.package] version`, add the CHANGELOG entry, merge.
2. `~/.local/share/godspeed-ops/release-macos.sh` (signs, notarizes, staples; writes
   `target/package/manifest.json` last via `scripts/release-manifest.py`).
3. Upload with the tag `v<version>` (the updater pins artifact downloads to that tag):

   ```sh
   cd target/package && V=<version>
   cp godspeed-$V-macos-arm64.dmg Godspeed-macos-arm64.dmg
   shasum -a 256 godspeed-$V-macos-arm64.dmg godspeed-$V-macos-arm64-app.tar.gz \
     Godspeed-macos-arm64.dmg > SHA256SUMS
   gh release create v$V --title "Godspeed Code $V" --notes-file <notes> \
     godspeed-$V-macos-arm64.dmg godspeed-$V-macos-arm64-app.tar.gz \
     Godspeed-macos-arm64.dmg SHA256SUMS manifest.json
   ```

   `manifest.json` and the `-app.tar.gz` are what installed apps update from; a release without
   them is invisible to the updater (it keeps offering nothing, or the previous release).
   Don't mark a release "latest" before all assets are uploaded (`gh release create` with files
   uploads first, then publishes).
4. Linux assets arrive on their own: `gh release create v$V` creates the tag, the tag push runs
   `.github/workflows/release-linux.yml`, which builds `godspeed-$V-linux-x86_64.tar.gz` and
   `godspeed-$V-linux-aarch64.tar.gz` (`scripts/package-linux.sh`, Ubuntu 24.04 runners, so
   glibc 2.39+), waits until the release exists, and uploads both tarballs, a `.sha256` for each
   and `manifest-linux.json` with `GITHUB_TOKEN` (no other secrets). Takes 30–60 min after the tag.
   If it failed or the release was created later than 2 h after the tag, rerun it from Actions →
   "Linux release" → Run workflow with the tag.

   Two manifests, one per uploader, so neither clobbers the other: `manifest.json` (written on the
   Mac, macOS files only, unchanged) and `manifest-linux.json` (written by the Linux job). Linux
   installs read `manifest-linux.json` first and fall back to `manifest.json`
   (`zeron_update::fetch_latest`); a manifest that lists files but not this platform's tarball is
   treated as "not published yet" and never downloaded unverified, so a Linux server waits out
   the gap between the Mac upload and the Linux job. Headless engines on servers update
   themselves from it when idle (no live runs or terminals), and "Run on a server" installs from
   the same assets after checking the sha256.


- `v0.5.0` published 2026-10-18 (tag on `4d5bbfb8`): Mission Control, receipts, usage cockpit,
  tachometer, continue on another account. Never swap the bundle under a running app: it cost the
  running copy its removable-volume (Lexar) access. Always stage + swap-on-quit.
- `v0.4.0` published 2026-10-17 (tag on `bd7fa2fc`): inbox, worktree environments, speedster Dock
  icon. Signed + notarized with `~/.local/share/godspeed-ops/release-macos.sh` (reads the ASC key
  from `~/.config/outbound/keys.env`, never prints it), then upload the dmg, the
  `Godspeed-macos-arm64.dmg` copy, the app tarball and `SHA256SUMS`. Installing over a running copy:
  `swap-on-quit.sh` in the same folder, loaded as a one-shot launchd job, swaps it in after quit.
- `v0.1.0` published 2026-10-16 (tag on `8841f23f`): ad-hoc signed, not notarized, Apple silicon
  DMG plus a copy named `Godspeed-macos-arm64.dmg` so the README's `releases/latest/download` link
  works, the app tarball and `SHA256SUMS`. For the next release: bump `[workspace.package]` version,
  add a CHANGELOG entry, run `scripts/package-macos.sh`, upload the same four assets.
- Zeron's upstream tags (fetched from `upstream`) reuse names like `v0.1.0`. The local copy of
  Zeron's `v0.1.0` was deleted so Godspeed's could be created; don't push upstream tags to `origin`.

## Cloud and iPhone app

Branch `feat/cloud-ios-prep` (2026-10-16) prepared Zeron-style sync on the owner's accounts without
deploying anything or choosing a sign-in method:

- Edge config targets Cloudflare account `f715952f0d5650529fe9401bcca31e2e` as worker
  `godspeed-edge` on workers.dev, R2 bucket `godspeed-blobs`. Zeron's routes, WorkOS client id and
  releases bucket are gone. Exact deploy commands, secrets and vars: `edge/DEPLOY.md`.
- iOS app rebranded to Godspeed: folders/project/scheme `apps/ios/Godspeed*`, bundle id
  `site.shrithan.godspeed.ios`, team `QPUV967B5U`, URL scheme `godspeed`, icon from
  `dist/godspeed.png`. It builds for the simulator and for arm64 devices (unsigned). Its
  `Endpoints` are placeholders (`.invalid` edge, empty client id), same as the desktop.

Remaining, in order:

1. **Deploy the edge** (`edge/DEPLOY.md`): enable R2, create `godspeed-blobs`, `wrangler deploy`,
   note the `godspeed-edge.<subdomain>.workers.dev` URL, check `/health`.
2. **Choose sign-in**: WorkOS (create a WorkOS app, redirect URI `godspeed://callback`, put the
   client id in the edge's `WORKOS_CLIENT_ID` and `wrangler secret put WORKOS_API_KEY`) or a
   pairing-code scheme (new edge routes + client code; nothing exists yet).
3. **Bake the edge URL and sign-in config into both apps**: desktop
   `crates/client/src/auth.rs` (`PRODUCTION_EDGE_URL`, `WORKOS_CLIENT_ID`) and
   `apps/godspeed/src/main.rs` (`DEFAULT_EDGE_URL`, env fallbacks `GODSPEED_EDGE_URL`,
   `GODSPEED_WORKOS_CLIENT_ID`); iOS `apps/ios/Godspeed/Shell/SignInViewController.swift`
   (`Endpoints`).
4. **APNs key for push**: Apple Developer → Keys (APNs), then `wrangler secret put APNS_KEY_P8` and
   `APNS_KEY_ID`. Team and topic are already set in `edge/wrangler.jsonc`.
5. **TestFlight**: create the App Store Connect app for `site.shrithan.godspeed.ios`, archive in
   Xcode and upload (`apps/ios/README.md`). Zeron's TestFlight workflow was not carried over.

Known leftovers: the edge's `/auth/cli/callback` page title still says "Zeron" (auth code was left
untouched on purpose); internal identifiers stay `zeron-*` (crates, the `zeron-preview://` image
link, `x-zeron-*` headers) because the core and the desktop speak them. The committed UniFFI
bindings (`apps/ios/Godspeed/Core/Generated/zeron_core.swift`) were stale after the desktop
rebrand (checksum mismatch at launch) and were regenerated on this branch.

## Before going public

- Rename the theme family still called after Zeron; replace the in-app Zeron logo SVG.
- Sign and notarize (done), set up a release feed (done: GitHub Releases, see Releases).

## Going public

Done on 2026-10-19: repo public, full CI triggers restored, links point at
ShriD5/godspeed-code. Remaining owner items are in `LAUNCH.md`.

## Owner to-dos

`muse login` for real Muse models; Settings → Accounts → connect Cursor (SDK sign-in is separate
from `cursor-agent login`); run the real chat import; eyeball the new UI (approval card, quit
warning, large paste chip, diff state per chat, Settings → Agents/Import).

## References

- Reference source clones live in `/tmp/refsrc` and vanish on reboot. Re-clone with
  `git clone --depth 1 https://github.com/pingdotgg/t3code /tmp/refsrc/t3code/repo` (and
  `Anil-matcha/open-dots`, `npm pack @muse-code/sdk`). Read only; never run their scripts.
- Upstream issue dumps and triage reports: `~/Developer/godspeed-issues/` (untrusted text).
