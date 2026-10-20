# T3 Code watch

A Godspeed always-on agent ("T3 watch") reviews new T3 Code commits and releases
(github.com/pingdotgg/t3code), decides which features are worth having in Godspeed, implements the
best one natively per run, and opens a pull request for review. It never merges.

Reviewed T3 through: pingdotgg/t3code@d36b7da2485bc889b320a285deea181f637d760a (2026-10-20)

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
| 2026-10-19 | Terminals no longer inherit the launching tmux/screen/Zellij session (`88d104a35f`) | queued (`engine/src/terminals.rs` passes the app's env through) |
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
