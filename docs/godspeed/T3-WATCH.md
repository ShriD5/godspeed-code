# T3 Code watch

A Godspeed always-on agent ("T3 watch") reviews new T3 Code commits and releases
(github.com/pingdotgg/t3code), decides which features are worth having in Godspeed, implements the
best one natively per run, and opens a pull request for review. It never merges.

Reviewed T3 through: pingdotgg/t3code@79f1fadfc6 (2026-10-31)

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
| 2026-10-31 | Claude accounts show the new plan names: Max 5x is Max, Max 20x is Ultra (`bf270e74e4`) | PR #TBD |
| 2026-10-31 | The context meter marks where Claude actually auto-compacts (window minus 33k) and fills against the limit (`159a914d6b`, `961026c6c9`) | queued (Godspeed's meter has no auto-compact mark; needs the threshold from the Claude driver) |
| 2026-10-31 | Pin favourite projects and archive old ones (`ffaa1dba76`) | queued (Godspeed pins chats within a project, not projects) |
| 2026-10-31 | Commands that exit 0 no longer show as failed (`469afd8b53`); cancelled ACP tool calls stop showing as running (`076aacca16`); Claude NotebookEdit names the notebook (`3a9640c3eb`) | queued (check our command and ACP tool rows for the same cases) |
| 2026-10-31 | Pick the model and its effort from one control (`c96c1b69f5`); picker Show more row (`2359643cff`); hide Ultrathink from Claude effort (`85bd3f2a2a`) | skipped (Godspeed's compact picker already shows model, effort and fast traits together) |
| 2026-10-31 | Side chats beside the main thread (`50c1c5dc53`) | skipped (Godspeed has side chats) |
| 2026-10-31 | Claude MCP elicitations on the approval card (`ff55c19baa`) | skipped (Godspeed already answers Claude elicitations from an input card: `answer_elicitation`) |
| 2026-10-31 | Message bubble shapes for media, files, pasted text and stacked bursts (`b8397e5caf`, `1da41f9499`, `e57ae5a0bf`, `f012fa210d`, `d6d2433609`); GitHub star prompt (`d92c437baa`) | skipped (T3's web look) |
| 2026-10-31 | ~370 other commits: Pi/Codex/OpenCode/Antigravity/Cursor fixes, Markdown and timeline perf, Bitbucket/GitLab/Forgejo/Azure fixes, SSH, preview browser, usage pricing, mobile, CI and release | skipped (internal/minor) |
| 2026-10-30 | The thread details card shows 5-hour, weekly and context limits (`310c128552`, `d7935f135f`) | PR #68 |
| 2026-10-30 | Stop always completes, and an agent that ignores it can be force stopped (`6b1245cced`, `36d4d711b7`, `6d12bb1e9f`) | queued (check Stop on a Claude/Codex run that ignores the interrupt; Godspeed has no force-stop) |
| 2026-10-30 | See and stop the dev servers a thread started (`a27ec88bfa`) | queued (with the T3-GAPS process-list row) |
| 2026-10-30 | Claude command approvals show the command, not Claude's description (`5eebf77b37`); Open diff shows Claude edits as a diff (`eb565e13ab`, `441a4ebd65`) | queued (check the Bash approval card and edit tool rows) |
| 2026-10-30 | Turn a No project thread into a project (`58e60bf5e8`) | queued (no move-to-project action in `crates/ui`) |
| 2026-10-30 | Review local commits in the Diff pane (`5ef56853ef`); PR diff hide-tests filter (`a5448f4c50`); shift-click marks PR files viewed (`ff424802ed`) | queued (see T3-GAPS diff and PR rows) |
| 2026-10-30 | Compaction row reads "Compacting for 12s" (`3ef1610079`) | queued (check the Claude compaction row's wording) |
| 2026-10-30 | Parallel worktree launches no longer fail on the `.git/config` lock (`3b22620a5c`) | skipped (Godspeed already serializes creation and its config marks per repository: `Repos::worktree_lock`) |
| 2026-10-30 | Fleet switch between Stable and Nightly (`f2943ee90e`); release notes in the update popover (`b5fc7bedfe`) | skipped (needs a second release feed; T3-GAPS "Update track" and "Release notes" rows cover it) |
| 2026-10-30 | Linked environments, @-mention a machine, remote delegate wake-back (`4005759adf`…`5e9d56416f`, `4ab7084aa7`, `0affd28db2`, `96a330c957`) | skipped (T3's peer links; Godspeed delegates across devices with `create_chat` `device`; wake-back is the existing T3-GAPS row) |
| 2026-10-30 | Simlock simulator leasing and EAS sessions (`be6ff17f51`, `be7d86fa66`, `812c0a71c7`, `d07c0a7c7b`) | skipped (Godspeed has no device panel yet; T3-GAPS Devices row) |
| 2026-10-30 | Provider fallback when out of usage (`861a51a6c4`); 1Password secret providers (`7a4f5b6188`, `97e35bcf23`, `ee81ea4ce8`) | skipped (account fallback out of scope; secrets are a Labs flag in T3) |
| 2026-10-30 | Boring sidebar folders (`4e4d4ba395`, `001d16e969`, `80fcb7f58f`, `5211acfe95`); multi-project filter (`c38b265adc`); custom snooze options (`93a9e079b1`, `c3858bf1f7`); F6 focus cycle (`2525252c96`); back/forward swipe (`476590d0ad`) | skipped (Godspeed has a project filter, snooze presets and back/forward; F6 is the existing T3-GAPS row) |
| 2026-10-30 | ~300 other commits: streaming and timeline perf, server query/startup perf, React effect removal, mobile/Android, GitCafe/GitLab/Forgejo, guest links, Antigravity/Cursor/OpenCode/Copilot fixes, `t3 service protect`, Electron bump, credential redaction, CI and release fixes | skipped (internal/minor) |
| 2026-10-27 | A thread whose worktree was deleted recovers on the next message (`0604547a3a`) | PR #41 |
| 2026-10-27 | Markdown previews hide a leading YAML frontmatter block (`f81c4cd79f`) | queued (`files/markdown_preview.rs` shows SKILL.md frontmatter as a rule and a setext heading; mask it with spaces so task toggles keep their offsets) |
| 2026-10-27 | ACP session notices show as notices, not reply text (`323d9611ff`) | queued (`acp/normalize.rs` drops `notice` updates; needs a notice event and row) |
| 2026-10-27 | Steer works while a compaction runs (`ad068d7c93`) | queued (check steering during a Claude /compact) |
| 2026-10-27 | Stopped runs read as muted, not errors (`f3b058cd6a`) | skipped (T3 web row styling; Godspeed run receipts already show "Stopped" in the normal tone) |
| 2026-10-27 | Claude MCP registration timeouts keep the native session (`fe867b86c8`) | skipped (T3 error tagging; see the 2026-10-26 MCP retry row) |
| 2026-10-27 | T3 MCP tool schemas cut by a third, 18 tools merged (`ed44f251d8`, `bc813670d9`) | skipped (T3's own orchestrator tools) |
| 2026-10-27 | Thread state at the end of the timeline (`bd3287b640`); spoken model labels (`56695ae3ec`); new-model badges expire (`0790e3891b`); Cmd+W closes sidebar tabs (`a86c05f977`) | skipped (T3's web look and model catalog) |
| 2026-10-27 | Warn before a second Claude/Codex account (`560ddb14d9`); Vercel fx provider (`6df241d23a`); inline charts prompt (`d2c440312e`, `c294d63b21`); PDF search (`becf853def`) | skipped (account fallback out of scope; T3 providers and viewers; Godspeed has `show_visual`) |
| 2026-10-27 | ~120 other commits: React effect removal, mobile/Android, PR screens, evals, telemetry, cloudflared pin, settings pages, CI | skipped (internal/minor) |
| 2026-10-26 | Forked threads get titles from the history they inherit (`a6aca345d5`) | PR #34 |
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
