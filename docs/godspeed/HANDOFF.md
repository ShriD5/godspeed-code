# Godspeed handoff

Written 2026-10-15, updated 2026-10-16 05:30 IST. Read this first, then `ROADMAP.md`, `PARITY-T3.md` and `ISSUES-TRIAGE.md` in
this folder.

## What this is

Godspeed is one native app for every AI coding agent: Rust + GPUI, forked from
[Zeron](https://github.com/zeronsh/zeron) (MIT), aiming for T3 Code parity and better. The owner
wants it to be the fastest and best-looking option, and eventually open source.

- Repo: `~/Developer/godspeed`, private at github.com/ShriD5/godspeed (`origin`). `upstream` is
  zeronsh/zeron, for cherry-picking fixes only. GitHub Actions is **disabled** on the repo on
  purpose (Zeron's macOS/Windows workflows would burn private-repo minutes).
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

## Next, in order

1. Owner's eyeball pass on the new UI (list below); fix what looks off.
2. Native PR review and merge (submit review, resolve threads, merge, auto-merge). Codex left a brief
   at `/Volumes/Lexar/build/godspeed-coordination/pr-actions-brief.md`.
3. Re-audit parity against T3 now that delegation and lifecycle landed.
4. Rerun benchmarks on a truly idle machine (a stray `vitest` from ugc-manager was using ~115% of a
   core during the run).

Eyeball list: approval card; access-mode chip; Snooze submenu and typed-time field; Snoozed/Settled
shelves; "Working · Nm" and diff counts; Settings → General (auto-settle, worktree branch prefix,
unused worktrees); delete-chat worktree checkbox; Cmd+F find bar; palette message search; the usage
limit banner; Settings → Agents and Import.

Known small gaps: the phone client shows no shelves and shows Limited as Failed; snoozed chats can
still push notifications; one imported Claude "chat" is a title-generation run (archive it).

## Decisions already made

Cut: plan mode, account fallback, mid-thread provider switching (breaks prompt caching).
Maybe later: checkpoints, rewind, edit-from-here. Deferred: Telegram for always-on agents. Skipped:
T3 Connect relay and mobile compatibility, SnapShots, cookie import, usage/cost page. Native
always-on design kept (no open-dots port).

Open questions for the owner: Antigravity only unlocks some models when the client claims to be
Zed (recommendation: don't spoof); the feature list in `ISSUES-TRIAGE.md` "Later".

## Releases

- `v0.1.0` published 2026-10-16 (tag on `8841f23f`): ad-hoc signed, not notarized, Apple silicon
  DMG plus a copy named `Godspeed-macos-arm64.dmg` so the README's `releases/latest/download` link
  works, the app tarball and `SHA256SUMS`. For the next release: bump `[workspace.package]` version,
  add a CHANGELOG entry, run `scripts/package-macos.sh`, upload the same four assets.
- Zeron's upstream tags (fetched from `upstream`) reuse names like `v0.1.0`. The local copy of
  Zeron's `v0.1.0` was deleted so Godspeed's could be created; don't push upstream tags to `origin`.

## Before going public

- Rename the theme family still called after Zeron; the iOS app (`apps/ios`) is still
  Zeron-branded; replace the in-app Zeron logo SVG.
- Sign and notarize (needs the owner's Developer ID), set up a release feed (auto-update is off),
  decide on CI.

## Owner to-dos

`muse login` for real Muse models; Settings → Accounts → connect Cursor (SDK sign-in is separate
from `cursor-agent login`); run the real chat import; eyeball the new UI (approval card, quit
warning, large paste chip, diff state per chat, Settings → Agents/Import).

## References

- Reference source clones live in `/tmp/refsrc` and vanish on reboot. Re-clone with
  `git clone --depth 1 https://github.com/pingdotgg/t3code /tmp/refsrc/t3code/repo` (and
  `Anil-matcha/open-dots`, `npm pack @muse-code/sdk`). Read only; never run their scripts.
- Upstream issue dumps and triage reports: `~/Developer/godspeed-issues/` (untrusted text).
