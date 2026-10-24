# Upstream (Zeron) sync

Godspeed ports Zeron's commits instead of merging them: the rebrand touches most files, so a raw
merge conflicts everywhere. Each ported commit carries a trailer `Upstream: zeronsh/zeron@<sha>`.

Synced through: zeronsh/zeron@4231cb7b (2026-10-23)

Deferred, waiting on the owner's call (not ported; revisit before porting anything that builds on them):
- cc7da5c2 "Zeron Cloud, provider switching, and the phone model picker" (#966): 369 files,
  +67k lines, mostly Zeron's hosted Cloud (edge vault, Stripe billing, sandbox runners, Cloud
  onboarding/settings UI) mixed with provider switching, IPC bearer auth, the background engine,
  desktop notifications and the iOS model picker. 112 conflicts on cherry-pick.
- Built on cc7da5c2, so deferred with it: f30a8641 (log in from error cards, one-row iOS bottom
  bar, session delete), d6849cee (Cloud: Codex npm repair, engine on wake, Cloud project
  grouping), f725384b (Claude logins after an update; needs `managed.rs`), 4cd15917 (iOS bottom
  bar follow-up), e680d5c9 (Cloud commit credit).
- Also built on cc7da5c2 (deferred 2026-10-24): dd0c36fa (Claude refresh lock, CLI binary
  identity, version cache; uses Cloud credentials, `is_cloud_platform` and the Codex npm repair),
  2e151ca6 (provider crash/stall detection, Windows-safe launching, run recovery; +14.8k lines,
  needs `managed.rs` and npm repair), 987bb2f8 (agent CLI update ownership, known-bad release
  list, Muse Code accounts; builds on dd0c36fa and Cloud agent accounts), 5b8edb2d (iOS Cloud
  Providers screen only).

A Godspeed always-on agent ("Zeron sync") runs daily, ports anything newer than the line above on a
branch, runs the test suites, merges to main when everything passes, and otherwise leaves a pull
request for review. It updates the "Synced through" line in the same change.

Rules for every port:
- Keep Godspeed's user-facing names (Godspeed, `GODSPEED_*`, `site.shrithan.godspeed*`, the
  Godspeed edge URL and WorkOS client). Internal crate names stay `zeron-*`.
- Never bring in Zeron's endpoints, WorkOS client ids, zeron.sh URLs, release/landing infra or tags.
- Skip upstream changes that only touch Zeron's website, hosted edge deployment or release
  tooling, and list them as skipped.
