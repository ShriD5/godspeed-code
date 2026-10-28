# Plan: the five things that bring users over

Written 2026-10-17 from two research passes (user pain on Reddit, Hacker News, X and GitHub issues;
a competitor scan of T3 Code, Zeron, Conductor, Orca, Superset, Paseo, the Claude and Codex apps,
Cursor and others). Each phase ships as its own release.

## How we work

- One feature per agent, in its own git worktree and build cache (`cp -c -R` clone on the Lexar
  drive). At most two builds at once, `CARGO_BUILD_JOBS=4`.
- Each phase starts with a short design section in the PR description, built on what Godspeed
  already has. Tests cover real behaviour (lifecycle, persistence, failure paths).
- The overseer reviews, merges to `main`, runs the suites, adds the new manual checks to
  `TEST-CHECKLIST.md`, and cuts a notarized release.
- The always-on agents keep running alongside: Zeron sync (15:00) and T3 watch (17:00).

## Phase 0: measure the headless usage cost (half a day)

Reports say `claude --print` / SDK runs use about 3× more of the 5-hour limit than interactive use
(claude-code#97074). Godspeed drives Claude with `--print`, so if true, every Godspeed user pays it.

- Run the same scripted task N times in each mode on one account: headless (Godspeed's driver)
  and interactive (the real `claude` TUI in a PTY). Read the 5-hour utilisation from the account
  usage probe (`crates/engine/src/agent_accounts/usage.rs`) before and after each run.
- If the gap is real: prototype an interactive driver that runs the TUI in a PTY for input and tails
  the session transcript under the Claude config dir for events (the chat importer already parses
  that format). Ship it as the default Claude mode if it keeps feature parity (approvals, steering,
  subagents, images), behind a setting otherwise.
- If not: publish the measurement; it is still a useful post.

## Phase 1: "Needs you" inbox (2–3 days)

One calm queue for every agent instead of babysitting ten chats.

- Engine: an attention service that merges every chat's state (awaiting approval or input,
  finished and unseen, failed, limited) and always-on agent runs into one stream
  (`WatchAttention`), ordered by risk and age.
- Risk ranking for approvals: read-only commands and in-workspace edits are low; network, git
  push, deletes outside the worktree, package publishing and secrets access are high. Low-risk
  items can be approved in a batch; high-risk ones always show the exact command or diff.
- Per-chat question timeout (setting): after N minutes, answer with the chat's default or park it,
  so unattended runs never stall forever.
- UI: an Inbox entry at the top of the sidebar with a count, keyboard-first (j/k to move, a to
  approve, d to decline, enter to open). The iPhone app and Telegram get the same queue; Telegram
  sends one digest instead of a message per item.
- Reuses: the approval respond path, session `awaiting_approval` / background-work flags, the
  always-on inbox (folded into this one), notifications.

## Phase 2: worktrees that work in parallel (2 days)

- Extend `godspeed.json` (today: project actions and `run_on_worktree_create`) with an `env`
  section: files to copy into new worktrees (`.env*`), a port block allocated per worktree and
  injected as variables, setup and teardown commands with template variables (`{{worktree}}`,
  `{{port}}`, `{{branch}}`) for things like cloning a database or schema, and a branch-name template.
- Sleep a worktree: stop its terminals and dev servers to free RAM, wake on open.
- Multi-repo workspaces: one project that spans several repos, each with its own worktree, after
  the single-repo pieces land.

## Phase 3: close the laptop, the work keeps going (3–4 days)

Godspeed already has the headless engine, chats hosted on another device, and cloud sync.

- Linux release artifacts on every release (`scripts/package-linux.sh`), plus an install script.
- "Run on a server" wizard in Settings → Devices: SSH host → install Godspeed headless → sign in →
  start the background service → the server appears as a device you can start chats on.
- Reliability: reconnecting a viewer never restarts or clobbers a running session; the phone,
  desktop and Telegram hand a chat back and forth without "already has a writer" errors. Soak test
  with a long run across lid-close, Wi-Fi drop and engine restart.
- Later: a hosted option on Cloudflare Sandbox for people without a server.
- Needs from the owner: a small Linux server (e.g. a Hetzner VPS) for testing.

## Phase 4: your limits go further (1 day, plus phase 0's result)

- Usage cockpit: live 5-hour and weekly usage per account and provider in one place, with the
  reset time; the composer ring links to it.
- "Continue on another account": an explicit button on a limited chat that forks it onto another
  signed-in account or provider with its context. Never automatic (switching mid-thread loses the
  prompt cache, so it stays a user decision).
- Auto-resume at reset already exists.

## Phase 5: PR autopilot (3 days)

Status (2026-10-28): review and merge from the PR pane, and the polling PR watch (failed checks,
review comments, conflicts wake the chat; Needs-you items; opt-in auto-merge when green and
approved) are shipped (#56). Webhook triggers remain.

- Native PR review and merge in the PR pane: submit a review, reply to and resolve threads, merge
  and auto-merge, through the existing `gh` wrapper (`crates/engine/src/source_control.rs`).
- Autopilot per PR (opt-in): watch checks; when one fails, queue a fix prompt into the PR's chat
  with the failing log; when a reviewer comments, queue the comments; when checks are green and
  approvals are in, merge. Every step shows in the inbox.
- Triggers: polling now, GitHub webhooks into the always-on webhook endpoint later.

## Owner inputs

- APNs key (`.p8`) so the inbox can push approvals to the iPhone.
- App Store Connect app record for TestFlight.
- A Linux VPS for phase 3.

## Order

Phase 0 and phase 1 start together. Then phase 2, phase 3, phase 4 (with phase 0's outcome) and
phase 5. Roughly two weeks of wall-clock time with two agents at a time.
