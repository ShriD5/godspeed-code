# T3 Code watch

A Godspeed always-on agent ("T3 watch") reviews new T3 Code commits and releases
(github.com/pingdotgg/t3code), decides which features are worth having in Godspeed, implements the
best one natively per run, and opens a pull request for review. It never merges.

Reviewed T3 through: pingdotgg/t3code@071239804e375078dd3fae3b14704b4def57e048 (2026-10-26)

## Rules

- Port the idea, not the code: T3 is TypeScript/Electron, Godspeed is Rust/GPUI. Fit Godspeed's
  architecture and style; keep the UI consistent with what exists.
- Out of scope (decided): plan mode, rewind/checkpoints, account fallback, mid-thread provider
  switching, T3 Connect relay and T3 mobile compatibility, SnapShots, browser cookie import, usage
  and cost page.
- Skip T3-internal work (Electron, React, its relay, its CI), and anything Godspeed already does as
  well or better (check `docs/godspeed/PARITY-T3.md` and the code).
- One feature per pull request, with tests for real behaviour.

## Ledger

Newest first. Status: `PR #n`, `queued` (worth doing, not yet built), `skipped (reason)`.

| Date | T3 change | Status |
|---|---|---|
| 2026-10-26 | Forked threads get titles from the history they inherit (`a6aca345d5`) | PR #33 |
| 2026-10-26 | Terminal drag selection scrolls faster the farther past the edge (`313ca2715f`) | queued (`selection_scroll_lines` in `terminal/panel.rs` tops out at 3 lines inside the grid edge) |
| 2026-10-26 | Resumed Claude turns retry a stalled MCP registration (`572cf74275`) | queued (check the Godspeed MCP tools after resuming a Claude chat) |
| 2026-10-26 | Parents hear when a delegated background run ends; running delegates tell the agent to end its turn (`0504c99205`, `557db6cb4e`) | queued (check a side chat finishing while its parent is parked) |
| 2026-10-26 | The branch picker refreshes the current branch when it opens (`5b35402623`) | queued (check the picker after `git switch` in a terminal) |
| 2026-10-26 | PDF attachments go to Claude as document blocks (`532023a241`); document text extraction for agents (`888a3d47d3`) | skipped (Claude reads attached PDF paths with its Read tool; inlining needs a session-wide 100-page budget) |
| 2026-10-26 | SVG attachments (`efea68679f`); audio playback and office/archive/sqlite/notebook file previews (`34e9c56b9b`, `d69810a97a`, `73afb57097`, `9d17a02782`, `34ead566d4`) | skipped (web viewer; Godspeed sends other files as path refs) |
| 2026-10-26 | Background compactions no longer flip threads to Working (`b6c326d12f`) | skipped (Godspeed has no idle compaction yet) |
| 2026-10-26 | New design on by default, Settings and Usage as modals (`3816296021`, `f40c7c6435`, `c3888da8b4`, `3faeb9e6f9`) | skipped (T3's web look; Godspeed keeps its own UI) |
| 2026-10-26 | Goals stop the running turn (`cc0b86ddf1`); T3 agent instruction cuts (`5f64c5eb29`, `3ed0688c10`) | skipped (T3's goals and orchestrator prompts) |
| 2026-10-26 | ~200 other commits: lint rules, React effect removal, mobile/Duo, task cache, observability, thread-open payloads, queued-edit fixes, crash screens, usage banner, CI | skipped (internal/minor) |
| 2026-10-25 | Worktree setup waits for the shell before typing, so a startup prompt cannot eat the command (`5d5b6e025c`) | PR #31 |
| 2026-10-25 | ACP terminal commands that ignore SIGTERM are force-killed (`c4fa4629eb`) | skipped (Godspeed's ACP client offers no terminal capability) |
| 2026-10-25 | ACP idle agent errors fail their turns (`5a969c17e7`) | queued (check an ACP agent that errors between turns) |
| 2026-10-25 | Codex pending requests are bounded after app-server stdout closes (`7769d709ce`) | queued (check a Codex crash mid-request ends the turn) |
| 2026-10-25 | Working holds only background work the agent waits on (`91ea0e18eb`) | queued (with the Agents-pill row) |
| 2026-10-25 | `ENABLE_TOOL_SEARCH=auto` no longer costs 10x cache writes (`24c80f34a1`) | skipped (only when a user sets auto; Godspeed adds few MCP tools) |
| 2026-10-25 | Command palette search finds options inside submenus (`e9d61252f6`) | skipped (Godspeed's palette has no submenus) |
| 2026-10-25 | Follow-ups steer the running turn by default (`b808f7fc6d`) | skipped (default send behaviour is the owner's call) |
| 2026-10-25 | Compactions T3 starts read as one quiet working row (`0345e852ed`) | skipped (Godspeed has no idle compaction yet; see the 2026-10-20 row) |
| 2026-10-25 | Share a thread with a guest who watches live (`fb76594d7b`) | skipped (T3 Connect relay, out of scope) |
| 2026-10-25 | Agents' shell-created PRs get linked (`dca46e2b24`); PR-watch, delegated-review and thread-tool fixes (`2da177c2a6`, `909e4c9482`, `003b64dfa5`, `a96cf0818e`, `e641761e2a`, `639dc20e43`, `3eb836664e`) | skipped (T3's forge watch and orchestrator tools) |
| 2026-10-25 | Thread-open and cold-connect payload cuts (`553203c57b`, `e19fe0837b`, `80455376f0`, `67c3afbd58`, `e57c093cdd`) | skipped (T3 projection store and wire format) |
| 2026-10-25 | ~70 other commits: mobile/Duo, Shiki to @pierre/highlights, screen readers, GitCafe, LAN devices, request_secret, Cursor/Grok/Antigravity fixes, analytics, comment trims, CI | skipped (internal/minor) |
| 2026-10-24 | Stop stops a thread's subagents again (`fbc6a08159`) | queued (check Stop while a Claude background subagent runs, with the steering rows) |
| 2026-10-24 | Interrupts an agent started no longer read "by user" (`7d87be3596`) | queued (check a chat interrupted through `interrupt_chat`) |
| 2026-10-24 | A fork's first /compact compacts its inherited history (`5b55036f8c`) | queued (check /compact right after a fork) |
| 2026-10-24 | Pasted images show their upload tile instantly (`4dcc244fe1`) | queued (check pasting a large screenshot into the composer) |
| 2026-10-24 | An instructed /compact no longer runs a bare compaction first (`16aaf5ba7d`) | skipped (Godspeed has no compact-before-send yet, PARITY-T3 §4) |
| 2026-10-24 | Cmd+K finds the right thread first, with indexed message search (`bb7783e295`) | skipped (`shell/thread_search.rs` already ranks message hits) |
| 2026-10-24 | Deleted threads' worktrees are cleaned up with their subagents (`171f8d880f`) | skipped (T3's subagent worktree model) |
| 2026-10-24 | Faster large terminal clipboard sequences (`9f660d3845`); streaming coalescing and allocation perf (`27e0c684c5`, `f7f02d779e`, `7de9535a6c`) | skipped (T3 history sanitizer and web runtime internals) |
| 2026-10-24 | Screen readers hear thread state changes (`eb1510d249`); readable blockquotes (`1df9c32943`) | skipped (web-only) |
| 2026-10-24 | ~55 other commits: forge refactors, PR list filters, Bitbucket, mobile/Duo, desktop quit and preview, usage, observability, relay, CI | skipped (internal/minor) |
| 2026-10-23 | Thread title instructions in settings (`51273020db`) | PR #27 |
| 2026-10-23 | Messages from other agents fold into one-line rows (`f054d1b801`) | queued (`agent_message_display` in `transcript.rs` keeps the full bubble) |
| 2026-10-23 | Claude approval feedback reaches the subagent that asked (`b7e802bdec`) | queued (check a decline-with-message on a subagent approval) |
| 2026-10-23 | agent-browser and Chrome no longer outlive the agent that started them (`1aa7179670`) | queued (with the helper-processes row) |
| 2026-10-23 | New threads no longer hang when a project's machine is offline (`56c365cc29`) | queued (check a new chat in an offline device's project) |
| 2026-10-23 | The sidebar follows the active thread after back/forward (`044f07257c`) | queued (check `NavigateBack` reveals the row) |
| 2026-10-23 | Cmd+K search hides subagents and favors recent whole-word matches (`4812dcad02`) | skipped (`shell/thread_search.rs` already scores word starts and breaks ties by recency) |
| 2026-10-23 | Cmd+B toggles the sidebar unless text is selected (`7d6b5bec87`) | skipped (the composer has no bold; `mod-b` always toggles) |
| 2026-10-23 | ~53 other commits: GitCafe/Forgejo PR watches, managed Codex auth, mobile, SQLite contention, usage, toasts, test cleanup, CI | skipped (internal/minor) |
| 2026-10-22 | Clones retry over HTTPS when the SSH key is refused (`a49d18cf3b`) | PR #29 |
| 2026-10-22 | The background service picks up JAVA_HOME from the login shell (`a7c22f12b7`) | queued (`shell_env.rs` hydrates PATH only) |
| 2026-10-22 | Pushes the remote refuses say why (`fb7f08782b`) | queued (check the push error text in `git_actions.rs`) |
| 2026-10-22 | Codex multi-file patches keep every path (`aacd1ee337`) | queued (check Codex file changes with several paths) |
| 2026-10-22 | A Claude agent stops when the server that started it dies (`5ffe045931`) | queued (with the helper-processes row) |
| 2026-10-22 | Idle compaction gets a global default and per-instance overrides (`d13fc84670`, `43879d9702`) | queued (with the idle-compaction row) |
| 2026-10-22 | Status refreshes skip expensive diff minimization (`370b9ae793`) | queued (check `diff_sync.rs` refresh cost) |
| 2026-10-22 | Mentions like `posts.$postId.tsx` or `[id].tsx` stay chips (`5c3721e20a`); typing a price no longer opens the skill menu (`1ee0241730`) | queued (check composer mention and `$` parsing) |
| 2026-10-22 | `../` links outside the workspace open as host files; folder links with a trailing slash resolve (`edce5f0e53`, `6de7da548d`) | queued (check transcript link resolution) |
| 2026-10-22 | Generated merge messages drop Claude Code's claude.com footer; commit prompts don't repeat CLAUDE.md (`b5a226e49c`, `1d9060b9af`) | queued (check generated commit messages) |
| 2026-10-22 | Approvals in full access say why Claude asked (`e749245387`) | skipped (the approval card already shows `decision_reason` and `blocked_path`) |
| 2026-10-22 | ~115 other commits: web refactors and render perf, iPhone Duo/mobile, GitLab/GitCafe/Forgejo/Azure, T3 Connect, OpenCode/Pi/Cursor/Antigravity fixes, MCP thread tooling, webhooks, telemetry, RTL text | skipped (internal/minor) |
| 2026-10-21 | git skips the macOS xcrun shim (`efdda5e521`) | PR #22 |
| 2026-10-21 | A stalled clone says "Stalled" instead of a frozen speed (`8fe4206dec`) | queued (`project_transfer.rs` clone progress has no stall timer) |
| 2026-10-21 | Composer stays usable during approvals; sending declines and redirects (`a93b03c8a5`) | queued (check the composer while an approval is pending) |
| 2026-10-21 | Stop on a stuck delegate always clears it (`6a6c84f990`, `163bdeaa11`) | queued (check Stop on a delegated chat that hangs) |
| 2026-10-21 | Steers split a finished turn's work instead of stacking under it (`a8bb769d65`) | queued (check transcript grouping after a steer) |
| 2026-10-21 | Selected held messages send when a thread is stopped (`96f09c6558`); idle orphaned follow-ups can be cancelled (`849b3f8948`) | queued (with the held-queue rows) |
| 2026-10-21 | Mermaid diagrams render natively (`fcb20e49aa`) | skipped (Godspeed already renders Mermaid, `markdown/mermaid.rs`) |
| 2026-10-21 | `t3 thread` CLI starts, messages and reads threads (`955b4cc801`) | skipped (Godspeed's MCP tools cover it) |
| 2026-10-21 | ~140 other commits: web refactors and test pruning, mobile, release/CI, scheduled-task webhooks, thread chips, Limits bars, storage cleanup | skipped (internal/minor) |
| 2026-10-20 | Add a project to another machine from Run on, .env files included (`effb4337`); it takes you there, no toasts (`54346a61`) | ported ("Add to <device>…" in the device picker and project menu; engine clone + ignored-file relay) |
| 2026-10-20 | Login-shell environment probes no longer take over the launching terminal (`7f63b10275`) | PR #19 |
| 2026-10-20 | Idle Claude threads compact before the prompt cache expires, beta (`5d3875d703`) | queued (larger; pairs with the compact-on-return gap in PARITY-T3 §4) |
| 2026-10-20 | Context meter marks the configured auto-compact threshold (`15c927899d`) | queued |
| 2026-10-20 | A message promoted into a stopping run stays queued (`10f7db7e38`) | queued (with the held-queue rows) |
| 2026-10-20 | Terminal history replay resets the modes a dead shell left on (`1125b40995`) | queued (check mouse/alt-screen after a shell dies, then reattach) |
| 2026-10-20 | Powerline separators render flush to cell edges (`3982f20ce4`) | queued (Godspeed draws them with the font; check by eye) |
| 2026-10-20 | Completions in threads you haven't opened are marked unread (`818c71d67d`) | queued (Godspeed has no unread state yet, PARITY-T3 §6) |
| 2026-10-20 | Running banner shows the full command when truncated (`060ac670a9`) | queued |
| 2026-10-20 | Composer continues and renumbers Markdown numbered lists (`d407f5443b`, `bac96aa3b2`) | queued (check the composer's list handling) |
| 2026-10-20 | PR +/- split into code, tests and docs (`92a6f6d731`, `962428df10`) | queued |
| 2026-10-20 | Helper processes no longer outlive the backend (`e85539ab59`) | queued (check engine shutdown leaves no agent children) |
| 2026-10-20 | Synchronized terminal updates paint atomically (`26338ea544`) | skipped (alacritty_terminal 0.26 already buffers DEC 2026) |
| 2026-10-20 | Claude shows automatic compaction while it runs (`fc21184afb`) | skipped (Godspeed handles `status: compacting` and notes the result) |
| 2026-10-20 | Nushell environments hydrate (`2fcefc9abf`); bounded slow terminal output (`3284143b45`) | skipped (Godspeed already has both) |
| 2026-10-20 | Idle terminals stop scanning the process table (`d1cac663a7`) | skipped (Godspeed never scans it) |
| 2026-10-20 | ~600 other commits: mobile, desktop/Electron, T3 Connect, EAS/device hosts, usage page, rewind/edit-from-here, provider-specific (OpenCode, Pi, Cursor, Grok, Antigravity, Muse) and React fixes, reverts | skipped (internal/minor or out of scope) |
| 2026-10-19 | Threads finishing together play one completion sound (`c1f9d20263`) | PR #9 |
| 2026-10-19 | Terminals no longer inherit the launching tmux/screen/Zellij session (`88d104a35f`) | PR #23 |
| 2026-10-19 | A message you send resumes a held queue; queued messages land after a resume (`62d98096ca`, `529d8063cc`) | queued (Godspeed has no held-queue state yet, see PARITY-T3 §4) |
| 2026-10-19 | A steered send no longer leaves a subagent task running (`afc12ab70a`) | queued (same as the `0b68a4a9c3` row) |
| 2026-10-19 | Failed attachment uploads no longer block sending (`073734c1e9`) | queued (check a failed attachment in the composer) |
| 2026-10-19 | Up-arrow recall skips automation and agent messages (`5569a42bff`) | skipped (Godspeed has no up-arrow prompt recall yet) |
| 2026-10-19 | Paths such as `/tmp/build.sh` no longer open the slash menu (`deb5b3941d`) | skipped (`invocation_token` in `composer.rs` already rejects them) |
| 2026-10-19 | A full Claude context window no longer shows as a usage limit (`fb9af9e57d`) | skipped (`harness/src/limits.rs` and `claude/normalize.rs` already handle it) |
| 2026-10-19 | fish no longer stalls 10s on startup (`45ad47df22`) | skipped (the terminal emulator answers DA1 itself) |
| 2026-10-19 | A failed run shows Failed, not Completed (`25dff68e01`) | skipped (rows already show Failed) |
| 2026-10-19 | Find, terminal output, file tree and code block render perf (`f3bb914efb`, `c6dfda8e56`, `1ea378c6f4`, `f36dadb782`, `219b3c2820`) | skipped (web renderer specifics) |
| 2026-10-19 | Provider fixes for Cursor, OpenCode, Grok, Antigravity, Muse, ACP and Codex (~25 commits) | skipped (minor; Godspeed drivers differ) |
| 2026-10-19 | Forge/PR watch, preview browser, devices, SSH, Tailscale, telemetry, mobile, Windows, web styling (~290 commits) | skipped (internal/minor) |
| 2026-10-18 | Step reasoning effort from the keyboard (`2993dfc069`) | PR #6 |
| 2026-10-18 | Model picker search lists newest models first (`ede3f2d6ba`) | queued |
| 2026-10-18 | Steering no longer ends running subagents (`0b68a4a9c3`) | queued (check Claude steer while a background subagent runs) |
| 2026-10-18 | Follow-ups sent while a thread compacts are queued (`a54a44c669`) | queued |
| 2026-10-18 | Command output streams live while it runs (`a5d97921f6`) | queued |
| 2026-10-18 | Agents pill counts background agents from earlier turns (`b7e376e96e`) | queued |
| 2026-10-18 | A slow but healthy Codex no longer shows as Unavailable (`cfafb4bb54`) | queued (check probe timeout) |
| 2026-10-18 | A snoozed thread wakes in the open chat view (`07cb902866`) | queued (check open-chat wake) |
| 2026-10-18 | Fail runs whose provider stream ends without a terminal (`e77a48e81a`) | skipped (unexpected exits already end the turn with a crash message, `harness/src/lib.rs`) |
| 2026-10-18 | One malformed ACP session update no longer ends the session (`559c4b1f21`) | skipped (`acp/normalize.rs` maps updates leniently from JSON values) |
| 2026-10-18 | Text generation no longer falls back to the flagship model (`45b5993a52`) | skipped (titles already use the cheapest model) |
| 2026-10-18 | PR links: open-on-GitHub setting, middle-click, drag into composer (`cd34df59a1`, `00cdceb196`, `b67ac65b74`, `8f026485df`) | skipped (minor) |
| 2026-10-18 | Subagent worktree, restart-wake and tool fixes (`e733a40295`, `fa1184d29e`, `4e85a93b20`, `1550058056`, `11ce406d2c`, `7111501be3`, `edc703b60a`) | skipped (T3's subagent worktree and workflow model) |
| 2026-10-18 | `t3 doctor` database reclaim and history compaction (`5752253818`, `2b6611ca01`, `65d390fc52`, `13aa882c22`, `eb1d80c667`) | skipped (T3 SQLite store) |
| 2026-10-18 | React Compiler and re-render work, web refactors, mobile design system, Windows, CI, relay/pairing, release (~490 commits) | skipped (internal/minor) |
