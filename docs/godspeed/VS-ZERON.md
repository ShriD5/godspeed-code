# Godspeed vs Zeron

Godspeed is a fork of [Zeron](https://github.com/zeronsh/zeron) (MIT). Zeron built the native GPUI
shell, the headless engine and most of the agent drivers, and Godspeed keeps all of that. This page
lists what Godspeed changes on top, and where Zeron is still ahead.

Compared against Zeron `main` at `3b2f7802` (2026-10-15), which is release v0.2.113 plus two
commits. Godspeed forked at `6ac88599` and has 146 commits since (about 51,000 lines added).
Every "Zeron: no" below was checked against Zeron's source at that commit, not taken from its
docs.

## At a glance

| | Zeron | Godspeed |
|---|---|---|
| Approvals | Agents run without asking: Codex is hard-coded to never ask, Claude auto-allows | Four access modes per chat (supervised, auto-accept edits, auto, full). Approval cards show the exact command or diff. A mode an agent can't honour is refused, never silently upgraded |
| Git | View diffs and PRs | Commit, push, pull, sync, init, publish and open PRs, with generated commit messages and PR descriptions in the repo's own style |
| Always-on agents | None | Cron, interval, one-off, webhook and file-change triggers, a memory file per agent, an inbox. Runs on the headless engine, so a server keeps them going |
| Telegram | None | Message a bot to run tasks, pick provider/model/project/access, schedule prompts and approve actions with buttons |
| Muse Code | Not supported | Native driver over Muse's own protocol |
| Moving in | Start from scratch | Import chats from T3 Code, Claude Code, Codex and Cursor. Claude Code and Codex chats continue their original sessions |
| Search | Chat titles | Local full-text index over every message, including imported history, plus Cmd+F find in a chat |
| Chat lifecycle | Archive | Settle, snooze, wake, mark unread, auto-settle, undo, working time on rows |
| Usage limits | Run fails | Chat shows as Limited and can resume on the same account when the limit resets, surviving restarts and sleep |
| Delegation | Create and message other chats | Same, and the parent chat wakes when a child finishes, fails or needs approval |
| Upstream bugs | Open | Fixes for 16 open Zeron issues and 36 open T3 Code issues (below) |
| Account and cloud | Zeron's hosted cloud and sign-in | Local-only by default, no account, nothing leaves the machine. Sync is opt-in on your own Cloudflare deployment |

## Features Godspeed adds

### Approvals and access modes

Zeron starts every agent with full permissions. Godspeed maps four modes onto each agent's real
permission system:

| | Supervised | Auto-accept edits (default) | Auto | Full access |
|---|---|---|---|---|
| Claude, Codex, Muse, OpenCode, Grok, Antigravity, Devin, Hermes | Yes | Yes | Yes | Yes |
| Cursor | No | No | Yes | Yes |
| Pi | No | No | No | Yes |

Cursor and Pi have no way to ask before acting, so their unsupported modes are greyed out and a run
in them is refused. Threads waiting on you show "Needs approval" in the sidebar with a notification,
and unanswered approvals wait instead of auto-denying.

### Git write actions

Zeron's Changes pane is read-only. Godspeed adds commit (with Commit & push and Commit & create PR),
fast-forward-only pull, non-forced push, sync, init and publish, plus a create-PR dialog that
follows the repo's PR template. Generated text matches the repo's commit style, and a setting (on by
default) strips agent credit lines.

### Always-on agents and Telegram

Neither exists in Zeron. Agents fire on cron (with time zones), intervals, one-off times, webhooks
(`POST /hooks/agents/{id}/{token}`, with payload templating) or file changes. Missed runs collapse,
runs never overlap, an agent pauses after three failures in a row, and schedules survive restarts.
Each run is a normal chat, so diffs, terminals and approvals work as usual.

The Telegram bot, modelled on [open-dots](https://github.com/Anil-matcha/open-dots), drives the same
engine from your phone. See the [README](../../README.md#features).

### Everything else

- **Muse Code** driver: streaming, tool and edit cards, todos, steering, interrupt, resume, usage.
- **Chat import** from four sources, idempotent, with continuable Claude Code and Codex sessions.
- **Instant search:** a local message index (worst case 3.5 ms over 43,000 messages in testing).
- **Chat lifecycle:** settle, snooze with presets or typed times, wake, unread, auto-settle, undo.
- **Usage-limit recovery:** crash-safe resume at reset on the same account; Stop or archive cancels.
- **Delegation wake:** a durable outbox, so a child's result reaches the parent exactly once.
- **Polish:** quit warning while agents run, large pastes as file chips, per-chat diff scroll and
  folds, worktree branch prefix, "delete its worktree too?", an unused-worktrees list.

## Zeron bugs fixed in Godspeed

Godspeed triaged every open Zeron and T3 Code issue ([ISSUES-TRIAGE.md](ISSUES-TRIAGE.md)) and fixed
the ones that apply. Zeron issues addressed: #132, #424, #431, #477, #485, #619, #668, #734, #764,
#768, #801, #845, #885, #892, #893, #913. Highlights:

- Leftover agent processes are cleaned up at startup, and OpenCode no longer orphans `serve`.
- "Done" alerts wait for background subagents and shells to actually finish.
- Worktree creation is locked per repo, and dirty worktrees are never force-removed.
- Git works with SSH host aliases and remotes not named `origin`, and never pops password prompts.
- Claude: compaction and "context full" are shown properly, blocked tools aren't marked done, MCP
  elicitation no longer hangs, effort respects `maxEffortLevel`.
- A large personal skills folder no longer hides a project's own skills.

## Performance

Godspeed runs the same engine and renderer as Zeron, so the goal is "no slower despite the extra
features", not a speed win. Headless engines, official release builds, six interleaved
fresh-profile trials each on an M2 MacBook Air:

| Median of 6 trials | Godspeed 0.2.0 | Zeron 0.2.113 |
|---|---:|---:|
| Cold start to ready | 65 ms | 65 ms |
| Idle memory (ready + 30 s) | 12.2 MiB | 10.6 MiB |
| Memory after 400 chats, 201 projects | 33.9 MiB | 33.2 MiB |
| List 201 projects / 400 chats (p50) | 1.95 ms | 1.74 ms |
| App bundle on disk | 143.8 MiB | 137.0 MiB |

Startup, memory after work, idle CPU and shutdown are a tie within noise. The one clear difference
is idle memory: Godspeed uses about 1.6 MiB more, for the agent scheduler, Telegram bot, message
index and the other added subsystems. Request latency gaps are smaller than the run-to-run noise.

Network: Godspeed made no connections at all. Zeron as shipped opens one HTTPS connection to
`edge.zeron.sh` about 20 s after launch, which matches its built-in release check (the payload was
not captured). The machine was not fully idle during the run. Method, raw data and limitations:
[BENCHMARKS-ZERON.md](BENCHMARKS-ZERON.md).

## Where Zeron is ahead

- **The iPhone app.** Zeron ships one. Godspeed's (sync, transcripts, approvals, voice) works but
  is not on TestFlight yet; build it from source. Sync itself is opt-in on both.
- **Platforms.** Zeron publishes Windows and Linux builds. Godspeed only ships a macOS (Apple
  silicon) build so far; the code still builds elsewhere but is untested there.
- **Release cadence and community.** Zeron ships almost daily with a larger contributor base.
  Godspeed picks up Zeron's fixes by porting them, so it can lag behind. As of 2026-10-16 it
  includes every Zeron commit up to `3b2f7802`.

If you want the hosted experience on every platform today, use Zeron. If you want approvals, git,
always-on agents and a one-step move from T3 Code on a Mac, use Godspeed.
