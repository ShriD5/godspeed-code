# Upstream (Zeron) sync

Godspeed ports Zeron's commits instead of merging them: the rebrand touches most files, so a raw
merge conflicts everywhere. Each ported commit carries a trailer `Upstream: zeronsh/zeron@<sha>`.

Synced through: zeronsh/zeron@fbb4083a4 (2026-11-25)

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
- Deferred 2026-11-06:
  - a8d1dc6e (#1241, "Updates that can't strand a device"): +4.5k lines, an update
    transaction in `zeron-update`, engine `update_trial`, harness update ownership. Builds on
    `managed.rs` (cc7da5c2), Zeron's update `channel.rs` (Godspeed has none) and 987bb2f8's
    package-owner plans; 17 conflicts. Follow-ups deferred with it: 2094602e and c3eb640c
    (Zeron's beta channel version for bundles and the engine).
  - zgx-only, deferred with zgx: the ~150 `apps/zgx` commits through 1b86c41c (panel home,
    zgx v3 panes/tabs, plugin and template security, updates UI, side chats), plus
    8f1c32d2's `zeron-theme` terminal ANSI legibility (Godspeed's theme crate has no
    `legibility`), 89701c65 and 36e7cf18 (`plugin-guard`), 4c08b1d1 (zgx beta script).
  - From 987bb2f8, only Muse's `-R<n>` build ordering in `version_numbers` came over (with
    302cdc98 and ce1de75e).
- Deferred 2026-11-07:
  - Provider updates through the install's owner: a58ce898, df57bd5b, 89a18dcf, 36e15cc6,
    54951f58 and f6d5d063 (with `scripts/provider-update-rig`). They build on
    `harness_updates/ownership.rs` and `process/cmd_shim.rs` from the deferred 987bb2f8 and
    2e151ca6.
  - Waiting on background work and sleep (`wait_on_background`, a chat's wait on synced rows):
    63c24a35, 8b31be76, ae4c54b8, 8c3f1043, 010da062, 4cc19a07, cbb8ab41, 566d2714 and
    c0695e35. They build on `harness/src/background.rs` (6aa4d5af, the deferred background
    tray) and `engine/src/rpc/agent_gate.rs` (8c8535b7, deferred with zgx).
  - zgx-only: every `apps/zgx` commit through 3a6c7aa3 (composer deck, Activity sidebar,
    Running tray, date picker, v2 recordings).
  - Partly ported: 3d9d08bf brought only its engine half (a steer carries a queued row's
    files); 76ca2e17's mock dropped the background-tray mode, and Godspeed has no
    `note_user_stop`.
- Deferred 2026-11-10:
  - a02a326b and 11420d69 (iOS Activity, Sleep, Settings → Activity; `Client::sleep_session`
    and `stop_waiting`, `SessionRow` sleep/wait fields): they need `Chat::sleep`/`sleep_state`,
    `SessionWaiting` and `StopWaiting` from the deferred wait-on-background chain above.
  - 533e89d6 (Linux "texture must exist" fix): pins zui 15ee708 and gpui-base cab50b0, which
    sit on the b564a34/59f8924 pins Godspeed hasn't taken (see c31124aa); a fork bump is the
    owner's call.
  - zgx-only: d5b65b78, 89b0d156 (plugin 3D scenes), 9fe52584 (plugin Open on GitHub).
  - Partly ported: 68c7ea28 (long chats on other devices) keeps Godspeed's relay without
    per-conn device checks and its own turn status report; respond_to_input still reads the
    whole transcript when it names a question older than the newest.
- Deferred 2026-11-17 (7437e3e9..09766b39: 888 commits, 627 non-merge, 333 of them
  `apps/zgx` only). 20 ported, the rest deferred or skipped. Full list:
  `git log --no-merges 7437e3e9..09766b39`.
  - zgx-only, deferred with zgx: every `apps/zgx` commit, zgx dictation (d9fbc6e5,
    17c30f63, 3d513729, cf6dda0e, 86105fa5, 63774eef), the engine `plugins` API
    (8e582ffc, 9bfdac98, 02a8ad24, b5f053f2, cdbd99f6, b2688c29, 564da441, e9a956ec,
    a11374a6, c7020611, 99056a1f, 89a8f081, 8ea7bf8c, 8a91a02d, adcac463, 1ff869f7,
    91d8a010), zgx Cloud and launch (08f06796, fe0f4f26, c44c6cf6, 0731c9e7).
  - Durable Cloud, R2 attachments and Cloud wakes (built on cc7da5c2): e3c83bd8, 029fea30,
    de157638, 6c4d8a0e, ae03b84a, c1eebcbc, 9c5ade8b, 9de23cf8, 415f1f24, 10019ccd,
    8085dc59, 9742ffcd, 7537ba9d, 6c19e751, ff655e0e, dc8ae9dc, 1372e45d, 6da445f5,
    c9b08217, 84f24f36, 321f14a1, 6bd35c92, d0a451a0, 451822be, db606643, d911ca96,
    d96f4af7, a6839b28, 6e8b4c8a and 5b3f50b3 (keep every send until removed), 3324f850
    and 2569857b (damaged chat store rebuilt), c7ea42df, 5ebeef3d, 538fd2b0, 9f43edfe,
    c10b5ac3 (banners; Godspeed has no engine `desktop_notify`).
  - The relay layer (chat generation 3, `chat2_host`) and everything built on it,
    including the snapshot-delta and registry write series: a9883e58, cbee9590, adb3a9ed,
    4fa6fb26, 17bb92de, 3c45b6d3, 2a56dca5, f85f3444, 83004279, eb2ff018, ba79b4df,
    25ece15a, f2197ca0, d2dce83c, d46de0ea, cee503f5, 0a49ea3e, f9608906, 93da4b72,
    6e473538, bf2c8e94, 09811cf5, 20bd70a5, bff39897, ec959a2f, 12c03242, 4257eb8c,
    bee81e89, ea2c29cc, f295d210, a3f6c352, d1d862f3, fe5f24a8, 5528c750, 631622ed,
    bb247410, e9e78b38, 3182a970, fe70ba30, 5f0f79d1, b987c50e, 2f2be538, d3391b2c,
    a0eca081, e8abc385, ba533912, aec924c1, 09b2c76b, 5ac47758, a2497307, 7c553f43,
    8602f872, cb3b88a0, 398cdabb, 5f299638, 4fc08058, 7ff0c663, a6336cb0, 7748351f,
    cd3d0020, 29ad3524, c004d547, ebf1c437, 6f4e0e51, a18277f5, b306399c, 3fb3dbbc,
    1783a9e1, 705b3084, bd0c5329, 6248f098, df4eecfe, 91033835, 8ada39b7, b36652c7,
    7a062eeb, 95d7bfc0, b9cf705c, da3ec3f4, fa55493f, 98199a5b, 41417142, e6b95954,
    952ec8a3, 3510f868, bd379611, 3ea858b9, 1a7dc2e4.
  - Update transaction and beta channel (with a8d1dc6e): e9d377c0, 645290f7, 080a3c39,
    644961c8, b5ebb7c7, dde81854, 132bc5f1, 68cc6347, 108b4444, a0a4efcc, 71144518,
    bb9ba180, 989c418c, ee37cc50, 7f01dad0, 6bf63430.
  - Worth dedicated ports (standalone, but conflict with Godspeed's own design):
    - 9290078c (deleting a chat ends its run and takes its children): conflicts with
      Godspeed's session wake/upsert, title regeneration and run start; also wires
      plugins and Cloud machine deletion.
    - 61d91d37 (preview scan via libproc instead of lsof/ps): Godspeed's
      `discovery.rs` differs; ~400 lines of FFI.
    - device_stats MCP tool: e1525b28, 325f478a, d45a30af.
    - Tool result images: c17262cf, bce5f058, a2115820, 3ef287e2, 204f9ca2, 1e910d91,
      067b9e7d, a4401fef.
    - Model picker provider names and Claude model ordering: f545d1d7, 33cc9d4e,
      d8f9a1ad, 60d85b85, 80b3d416, f5d0530e, f224da9e; hosted model list 7a1f03f7.
    - Provider accounts for Pi/OpenCode/Hermes, API keys: a26f5f4c, 6c7b1ac2, 24cae278,
      3d07f58d, 4ec5b50c, 59b2f1c4, adc82e90.
    - Upstream's Auto access mode: 4b983c6c, e934f876, 31ac609b (Godspeed has its own
      `Auto`; see 1295d577).
    - A project's name and icon belong to its repository: 446e4343, 04d5f345.
    - c1168b6b (a send taken before the open chat's publish): ported alone, its test
      still saw a send in a 3,000-entry chat wait 195 ms vs 17 ms; it needs the read
      cache series above (4fc08058, 398cdabb, b36652c7) to pay off. Its test e6b95954
      goes with it.
    - A revived turn waits for the IPC port: 3b434a68, a1847745, 69e03cbc, b6b79ab3,
      003b4feb, fd3bb08c (need `deferred.rs` and run recovery from 2e151ca6).
    - iOS/Android: 2e5a678a, 5986bf3b (Activity; need a02a326b), fe8be7fe, 1bc3bcdc,
      bindings regenerations 263fd8d8, 4e4faa64, f852eab0.
  - Partly ported: 6ab00cf5 without the provider-switch hold (cc7da5c2); 75fbbbb6's
    test without the deferred a2113ac8 Tool fields; 50ba9b78 with its test rewritten for
    Godspeed's access model; ac1700f2 without its update-status test hunks.
  - Skipped: version bumps (854248bd, 51d069c7, 53fa1720, 8f75cc28, 0733d73d, 703500ee,
    464377bb, 82502ace, 2d480e9f, e48dcbed, 55a5c392), edge-only, CI and release
    scripts, relay design docs, d0c77e9c (`apps/zeron`), 7cf2ba66 (zgx exe icon),
    0f9a7500 (`crates/cli` log rotation; Godspeed has no `crates/cli`), 2252f736 (fixes a
    codex `account_id` check Godspeed doesn't have; see adf87317), 58236762 (Linux
    release LTO), 749ed748 (a test for upstream's generic
    `TranscriptFrame::reset`, which Godspeed's isn't).
- Deferred 2026-11-18 (09766b39..c58f65e8: 150 commits, 124 non-merge, 49 of them
  `apps/zgx` only, deferred with zgx). 13 ported, the rest below.
  - Agents' Godspeed tools served from the engine over Streamable HTTP, not a process per
    agent: c48876c38, with 4f95a9bd6 and 509857e4f (endpoint limits), 5b5d6a04b (no token
    on the command line) and 4b0f499ef (no fsync of the private MCP config). Godspeed
    still passes `--mcp-config` on the command line and has no `mcp/src/http.rs`.
  - Idle agent stop (an idle agent process ends after 2 min, 30 s when no window shows
    its chat): c987be092, 519fec249, dd48d0360, ed0f2729d, 89b6f7baa, 94bb83933,
    74cd87f16, 7c6f3e6b8. Conflicts in sessions.rs/process.rs; worth a dedicated port.
  - Warm starts (an agent launched while the first message is typed): 4941b7b90,
    7661575d4, 9e44ed8f5, e737faf46 (need `managed.rs`; the UI half is zgx).
  - Agents die with their engine (pid registry, PR_SET_PDEATHSIG): f558b51af. Worth a
    dedicated port; builds on the Claude refresh lock (dd0c36fa).
  - The idle engine series (one wall-clock beat grid in `zeron_sync::wake`, OS wake
    notices, sleeping reapers and watchers): 9a11f98d7, 39277756e, 357cf2813, bd579e57d,
    413382fc7, a881538ca, 025bec869, 9237a0390, 7c8fa5edc, 18fc4b086, a9b4790e1 (relay
    layer), 8f8f39cff (needs 61d91d37), 20dadc810 and 69768a512 (upstream's tagged
    allocator wrapper), 8a8cdc607 and 7654168e6 (`idle_rig` needs `connect_ipc`).
    Ported from it: 45b79a8f1, 98924b823, c19d45922 (by hand), f1ecad286, 445f70a00.
  - Keep running after quit: 758e7a7a1, ebb7dd048 (zgx setting, engine `keep_running.rs`).
  - Titles and `first_word` tracing: 9c80a0e1c, dd460b304, 82ce7506f, e068e04a7 (need
    `wake_trace` and upstream's title trigger).
  - Commands on other devices, Cloud isolation: eb42145a9, 41135ffcb, 198324c09,
    c8ea12ceb, 7ca6d8577, 5a63a5803, 85d2dc3a7, e8069c403, 6069c35c9, 8206fe614 (need
    `agent_gate.rs`, the background band and Cloud).
  - b0e62255d (keep chats on this device while signed in): its engine half has no
    setting in Godspeed's UI yet.
  - c58f65e8a (iOS sidebar hide button): Godspeed's iOS toolbar (inbox, board) differs.
  - Partly ported: f38065825 brought only `CLAUDE_CODE_DISABLE_TERMINAL_TITLE` (its MCP
    instructions rewrite is Zeron's text); 8815fb235 brought `bounded_file` and the
    model-context hash only (its other readers aren't in Godspeed); e51676b29 without
    the deferred `tool_exchange`; 6191678a6 without its cut-off-turn test (Godspeed
    doesn't revive a turn cut off by a graceful shutdown).
  - Skipped: fee8cfa1b (upstream retires the GPUI app, which is Godspeed's), 8b2f4b02b
    (landing), 85c1fe2b0 (version bump), bc1ecbb3f and 4e86e6bcc (3 warm docs, then
    back to 12: no net change), 9a6c3d9e3 and f0ab203f6 (mock `/longthread`, `/stream`
    bench modes for Zeron's ademark rig).
- Deferred 2026-11-20 (c58f65e8..2e3513c6: 24 commits, 20 non-merge, 8 of them
  `apps/zgx` only, deferred with zgx: cef90597, 4aac725b, 2c965d7a, b1064c1b, 774dd72e,
  c5f65ea0, e393c4a8, 4820dd83). 1 ported (bc14b5b1, by hand), the rest below.
  - d3a76091e (a long chat reopened with rows held sends its newest entries first):
    builds on the deferred snapshot-delta series (`entry_sum`, `orderSum`, `known`).
  - e66369f48 (reopening a remote chat asks the room and its host for what it missed):
    the relay layer (`chat_relay.rs`).
  - 17d777fa0 (Grok Auto mapped to Grok Build's auto mode, Full access always-approve,
    reviewer's reason on approvals): built on upstream's Auto access mode (4b983c6c).
  - 890c4527e (Antigravity 1.5.0 hardening: step-boundary steering, stall watchdog,
    effort ladder, corrupt-session recovery, Neon thoughts; plus queued provider/model
    switches and the provider-switch seam): its tests and engine half need provider
    switching (cc7da5c2) and run recovery (2e151ca6). The Antigravity harness half is
    worth a dedicated port.
  - 8c5549ecb (Cloud: every built-in agent with every local login): Cloud (cc7da5c2).
  - Partly ported: bc14b5b12 brought the 3 s probe settle and test-run exclusion; its
    "environment read empty" half is about upstream's origin-marker cache, which
    Godspeed's cwd-based discovery doesn't have.
  - Skipped: version bumps (c7f63a53, 29ec0489, 22eb085c, 5ac8d75a), 81cbda60 (CI apt
    retry), 8fe624f8 (edge credit rate config).
- Deferred 2026-11-21 (2e3513c6..46f18fe4: 19 commits, 10 non-merge; `apps/zgx` only,
  deferred with zgx: 2c8001a8, 9fb81caa, 3f5381c1, 0dbdf725, 613accb2). 1 ported (865308ff,
  voice ort features), the rest below.
  - 21ec6f8b3 (Antigravity dynamic updates, opt-in Code Mode for Codex and Muse): +916
    lines over 100 files, half of them zgx; the engine half edits `warm_start.rs` (warm
    starts, deferred) and builds on the deferred Antigravity hardening (890c4527e). Its
    new proto field touches every engine/harness test initializer. Port with 890c4527e.
  - Skipped: version bumps (b7b1e2d3, 26000a74), 46f18fe4 (Cloud install script, edge).
- Deferred 2026-11-25 (b2ee6d1b..fbb4083a4: 33 commits, 30 non-merge). Nothing ported.
  - MCP servers (vault connections, engine gateway/OAuth/presets, per-agent "only Zeron's
    MCP servers", agents asking for MCP changes): d5ff0b1ad, 2f4768659, 848750301, 0150fde8e,
    60a7980b3, 173ae0b1c, a2fa2d757, fcaa7f04e, 1c46da29e, 38bc732b6, 20ce01edb, f4c151c84.
    They need the VaultConnections Durable Object and `warm_start.rs`/`managed.rs`, which
    Godspeed doesn't have. Worth a dedicated port as one feature if the owner wants MCP management.
    3c3eed924 (switch off Codex plugins' MCP servers) hangs off that per-agent switch.
    fbde94408 (`zeron mcp` CLI) also needs `crates/cli`.
  - Antigravity unified CLI cbbab5067 and its Windows fix ed97f42ec: these need
    `credentials.rs`, `agent_accounts` and `acp/ccpa_thoughts.rs` (890c4527e chain).
  - Cloud: 8801bf815 (Providers for Cloud), c765f256c (`managed.rs` wake tools).
  - `apps/zgx` only, deferred with zgx: a94e4e7dc, 9e9dff3b4, 0777a30a2, 39118ef52,
    768b25a86, 53a4637a6, fbb4083a4.
  - Skipped: version bumps e081e38e1, 296461969, 82a58c5f6, e56940314; c4b09b659 (edge).
- Deferred 2026-11-23 (46f18fe4..b2ee6d1b: 19 commits, 17 non-merge). Ported b5cfaaccd (Pi
  set_model refusal) and the `crates/mobile` half of 8141d7d9e (image-named attachment chips).
  - e595626b0 (Antigravity disconnects in Cloud Runner): the engine half is Cloud
    (`credentials.rs`, antigravity_proxy); the harness half needs the CCPA thoughts proxy
    (`acp/ccpa_thoughts.rs`) from the deferred Antigravity hardening (890c4527e). Port with it.
  - aad53585e (Cloud delegates childKind): Cloud.
  - `apps/zgx` only, deferred with zgx: 66d025b24, 7662482bf, e131c1ba0, 80b78c1e8,
    2fcd150ea, e88155990, 225078f9e, 7b2eae9f7, 55ca07782, 838478e24.
  - Skipped: version bumps (b289003b7, b2ee6d1b9), aec10cd12 (docs rewritten for ZGX).
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
