# Upstream (Zeron) sync

Godspeed ports Zeron's commits instead of merging them: the rebrand touches most files, so a raw
merge conflicts everywhere. Each ported commit carries a trailer `Upstream: zeronsh/zeron@<sha>`.

Synced through: zeronsh/zeron@35561ead (2026-11-03)

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
- Deferred 2026-11-04: zgx, Zeron's new interface (`apps/zgx`, a Luau-scripted shell with a
  design system, plugins, templates, themes and a beta update channel).
  - d898f936 (#1137, squash of 2d63bdb2, b721a994, a4520ca3, 40183b93, b4b8cf04, c00c9a28):
    2034 files, +324k lines, ~1780 of them in `apps/zgx`. Also touches about 90 engine
    files (the `plugins` module, update channels, shared CLI/launch/notify crates). The owner
    needs to decide whether Godspeed takes zgx at all.
  - Built on zgx, so deferred with it: 8c8535b7 (plugin and template consent, engine-held
    decisions, edge plugin keys), the whole of merge 35561ead (#1198: about 60 `apps/zgx`
    commits, plus 0a6aef02 theme tools and catalog in `zeron-theme`/MCP and c83937d3's
    `zeron-preview-label` ports list).
  - Engine halves of zgx features with no reader in Godspeed's UI yet: c90481c6 (a child
    chat's `childKind` side/subagent; the MCP would default delegated chats to `subagent`,
    which only zgx lists), a2113ac8 (redacted command output tails and exit codes kept in
    the chat doc; this moves the redactor into `zeron-proto`) and 5e927a01 (`ReadToolOutput`,
    needs a2113ac8). Worth porting together once Godspeed's transcript shows command output.
- Partly ported (2026-11-02): 2500e8b3 brought only the Muse crash-exit race fix. Its CI
  restructure (single ci.yml with admission/reuse, nextest profile, merged `tests/misc`
  binaries) is Zeron's CI; its term-link race fix needs the deferred `zeron-term-link` crate
  (dfbb6876); its git identity XDG fix lives in `credentials.rs` (cc7da5c2).
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
