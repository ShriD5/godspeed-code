# Upstream (Zeron) sync

Godspeed ports Zeron's commits instead of merging them: the rebrand touches most files, so a raw
merge conflicts everywhere. Each ported commit carries a trailer `Upstream: zeronsh/zeron@<sha>`.

Synced through: zeronsh/zeron@d643a2ca (2026-10-30)

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
- Also built on cc7da5c2 (deferred 2026-10-25): fd7d268c (iOS terminals, side chats and
  subagents; uses CloudStore, error-card sign-in actions and provider switching), with its
  follow-ups dfbb6876 (iOS direct WebRTC terminal link; moves remote terminals into a
  `zeron-term-link` crate), 3c97a05c (terminal glyphs) and 2ea4e3a0 (composer level with the
  sidebar toolbar); a9e6c48b (previews only for ports Zeron started; needs the terminal-to-chat
  association and `prepare_run` from cc7da5c2); 684becf0 and 8ae969ad (Android app on the
  mobile core; its screens use the Cloud and billing FFI, and it would need a rebrand).
- Partly ported (2026-10-26): 325384d8 brought over the agent-sender caption only; its
  provider-switch seam restyle and the engine's earlier seam write (`note_provider_switch`) wait
  for provider switching from cc7da5c2.
- Deferred 2026-10-27:
  - ec0897fe (a session left before its message lands shows its run): needs `session_stale`
    and `last_turn_outcome` from cc7da5c2.
  - b8ed9d9c (switch to turn native subagents off): built on the parked-session replacement
    by credential/binary identity (cc7da5c2, dd0c36fa), commit credit and the Cloud-era
    providers page.
  - 64a26d35 (questions show what was answered, close one unanswered): conflicts in ~10
    files with Godspeed's own approval questions, MCP elicitation and OpenCode forms; worth a
    dedicated port.
  - 00cc2e05 (remote chats show the sync, not an outage): rewrites the connectivity captions
    Godspeed replaced (`chat_sync_pill_caption`, StorageError) and uses Cloud presence.
  - e16cc3e9 (provider sign-ins past the preview room's device limit): builds on
    `run_with_wake` (iOS chain) and needs the matching edge change deployed.
  - bf6d71aa (settings search, Cmd+F): its index covers Zeron's settings (Cloud, billing,
    delegation, commit credit); needs a Godspeed index. 3d2f1e64 was ported without its anchor.
  - c31124aa (Linux WPE browser): needs newer zui/gpui-component pins (b564a34, 59f8924)
    than Godspeed's (361ff20, 701ca97); a fork bump is the owner's call.
- Deferred 2026-10-28:
  - 1295d577 (access modes and plan mode for every agent): Godspeed already ships its own
    access modes (`RunAccess` with writable roots, an extra `Auto` mode, the legacy
    sandbox/auto_approve pair, `crate::approval`) and already answers Muse's staged
    `approval/updated`. Upstream's parallel design (`access`/`plan` on ChatConfig and
    RunRequest, `Harness::access_modes`/`supports_plan`) conflicts in 55 files. Plan mode
    (Claude `plan`, Codex `plan` collaboration mode, OpenCode `plan` agent) and the MCP
    create_chat access cap are the parts Godspeed lacks; worth a dedicated port onto
    Godspeed's model.
- Deferred 2026-10-30:
  - 6aa4d5af (background processes: tray, read output, stop): Godspeed already publishes
    a `background_tasks: u32` count on the session row (delegation wake-ups, the busy
    indicator) under the same `backgroundTasks` wire key that upstream turns into a
    `Vec<BackgroundTask>`, so older peers would fail to read new rows. Needs a wire
    decision (new key, or keep the count beside the list) and a merge with Godspeed's
    count users; 26 files conflict.
- Partly ported (2026-10-27): f65c5bc6 and d8a14b35 brought the shared header parser and the
  desktop rows; their iOS/Android captions wait for inline marks and a workspace snapshot in
  the mobile layout (Android needs cc7da5c2).

A Godspeed always-on agent ("Zeron sync") runs daily, ports anything newer than the line above on a
branch, runs the test suites, merges to main when everything passes, and otherwise leaves a pull
request for review. It updates the "Synced through" line in the same change.

Rules for every port:
- Keep Godspeed's user-facing names (Godspeed, `GODSPEED_*`, `site.shrithan.godspeed*`, the
  Godspeed edge URL and WorkOS client). Internal crate names stay `zeron-*`.
- Never bring in Zeron's endpoints, WorkOS client ids, zeron.sh URLs, release/landing infra or tags.
- Skip upstream changes that only touch Zeron's website, hosted edge deployment or release
  tooling, and list them as skipped.
