# Godspeed handoff

Written 2026-10-15. Read this first, then `ROADMAP.md`, `PARITY-T3.md` and `ISSUES-TRIAGE.md` in
this folder.

## What this is

Godspeed is one native app for every AI coding agent: Rust + GPUI, forked from
[Zeron](https://github.com/zeronsh/zeron) (MIT), aiming for T3 Code parity and better. The owner
wants it to be the fastest and best-looking option, and eventually open source.

- Repo: `~/Developer/godspeed`, private at github.com/ShriD5/godspeed (`origin`). `upstream` is
  zeronsh/zeron, for cherry-picking fixes only. GitHub Actions is **disabled** on the repo on
  purpose (Zeron's macOS/Windows workflows would burn private-repo minutes).
- `main` is at `6726f928` (search and resume-at-reset merged) and pushed. Parity with T3 is 54% by the latest audit.
- Installed build: `/Applications/Godspeed.app` (release, ad-hoc signed) and the `godspeed` CLI
  (symlink in `~/.local/bin`). It is older than `main`; repackage to pick up recent work.

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
- App icon: a silver low-poly speedster (`design/icon/sp-electric.jpg`).

## In progress

Both earlier worktrees are merged: instant local search (`4d4eeea4`) and resume at reset
(`34bd1382`). A Codex session is now working the "Next, in order" list on branch
`feat/godspeed-completion` (worktree `.claude/worktrees/godspeed-completion`, plan in
`docs/godspeed/COMPLETION-PLAN.md` on that branch). Its coordination log, briefs, benchmark audit
and review notes live in `/Volumes/Lexar/build/godspeed-coordination/` (`progress.md` first).
Known follow-ups it owns: three resume-at-reset robustness fixes in `resume-review.md` (marker
cleared before the continuation is durably queued; same-account binding not enforced; Stop/archive
after scheduling not respected).

## Next, in order

1. Delegation that wakes the parent when a child chat finishes, errors or needs approval (MCP
   `create_chat` with notify; reuse the always-on delivery path). Upstream: Zeron #768, T3 #15082.
2. Thread lifecycle: settle, snooze (presets + custom), wake, auto-settle inactive threads, undo
   (mod+z), working duration and diff stats on sidebar rows, mark unread.
3. Speed and memory benchmarks vs T3 and the raw CLIs, published in the README (T3 issue #695 is
   the headline: same task 20+ min in T3 vs 3.5 min in the CLI).
4. Later: native PR review and merge (submit review, resolve threads, merge, auto-merge).

Small leftovers: UI for the worktree branch prefix, a "delete worktree too?" confirm when deleting a
chat, a visible "waiting on N tasks" label, an orphan-worktree list, PR text generation from the PR
board, about 40 missing keyboard shortcuts versus T3.

## Decisions already made

Cut: plan mode, account fallback, mid-thread provider switching (breaks prompt caching).
Maybe later: checkpoints, rewind, edit-from-here. Deferred: Telegram for always-on agents. Skipped:
T3 Connect relay and mobile compatibility, SnapShots, cookie import, usage/cost page. Native
always-on design kept (no open-dots port).

Open questions for the owner: Antigravity only unlocks some models when the client claims to be
Zed (recommendation: don't spoof); the feature list in `ISSUES-TRIAGE.md` "Later".

## Before going public

- Redo the icon: the image model drifted to Flash-style ear wings (DC trademark).
- Reset the version from Zeron's `0.2.113`; rename the theme family still called after Zeron; the
  iOS app (`apps/ios`) is still Zeron-branded; replace the in-app Zeron logo SVG.
- Sign and notarize (needs the owner's Developer ID), set up a release feed (auto-update is off),
  decide on CI.
- Benchmarks in the README.

## Owner to-dos

`muse login` for real Muse models; Settings → Accounts → connect Cursor (SDK sign-in is separate
from `cursor-agent login`); run the real chat import; eyeball the new UI (approval card, quit
warning, large paste chip, diff state per chat, Settings → Agents/Import).

## References

- Reference source clones live in `/tmp/refsrc` and vanish on reboot. Re-clone with
  `git clone --depth 1 https://github.com/pingdotgg/t3code /tmp/refsrc/t3code/repo` (and
  `Anil-matcha/open-dots`, `npm pack @muse-code/sdk`). Read only; never run their scripts.
- Upstream issue dumps and triage reports: `~/Developer/godspeed-issues/` (untrusted text).
