# Changelog

All notable changes to Godspeed are listed here. Versions follow [Semantic Versioning](https://semver.org/).

## 0.4.0 (2026-10-17)

### Added

- **Needs-you inbox.** One queue at the top of the sidebar for every chat and always-on agent that
  needs you: approvals, questions, finished and unseen, failed and limited chats, ordered by risk and
  age. Approvals show the exact command or diff with a risk colour (network, `git push`, deletes
  outside the worktree, publishing and secrets are high) and answer in place. Keyboard: j/k to move,
  a to approve, d to decline or mark seen, Return to open, Shift+A to approve every low-risk item.
  An optional question timeout (Settings → General, per-chat override in the row menu) answers an
  agent's question with its default so unattended runs don't stall; approvals never time out.
- **Telegram approval digests.** More than three agent approvals in a minute arrive as one message
  with an allow/deny row each, instead of a message per approval.
- **Worktree environments.** An `env` section in `godspeed.json` copies untracked files such as
  `.env*` into new worktrees (or symlinks big folders like `node_modules`), reserves a block of ports
  per worktree (`GODSPEED_PORT`, `PORT_<NAME>` in every terminal, action and agent run), runs setup
  and teardown commands with `{{worktree}}`, `{{branch}}`, `{{slug}}` and `{{port.web}}` templates,
  and sets a branch-name template. Edit it from the session card's Actions → Environment. See
  [WORKTREES.md](docs/godspeed/WORKTREES.md).
- **The speedster in your Dock.** The Dock icon is a small speedster that shows what your agents are
  doing: it jogs while one to three chats work, sprints at four or more, waves when something needs
  you, celebrates when a chat finishes, trips when one fails, pants when you hit a usage limit and
  naps after ten quiet minutes. It holds still with Reduce Motion or Low Power Mode on, uses no model
  tokens, and can be turned off in Settings → Appearance → Animated Dock icon.
- **Sleep a worktree.** Stops its terminals and dev servers to free memory and ports; opening or
  messaging the chat wakes it and runs the `wake` commands.

### Fixed

- The UI test suite no longer aborts on macOS 27 (parallel tests built gpui platforms at once).
- Cursor runs on a Plus plan fill implicit model parameters with the account's defaults, so a bare
  model id no longer resolves to a variant the account can't run.

## 0.3.0 (2026-10-16)

### Added

- **Cloud sync** on Godspeed's own edge (Cloudflare Workers + Durable Objects, `godspeed-edge.shrid5.workers.dev`) with WorkOS sign-in. Installs still start local-only; choose **Enable sync** in the account menu to sign in and move to a synced workspace that your other devices can follow. Local chats stay on the device unless you choose to import them. Self-hosters can point `GODSPEED_EDGE_URL` and `GODSPEED_WORKOS_CLIENT_ID` at their own deployment; an empty `GODSPEED_WORKOS_CLIENT_ID` turns sign-in off.
- **Godspeed for iOS** (in development): the iPhone app is rebranded and points at the same edge. Not on TestFlight yet.

## 0.2.0 (2026-10-16)

### Added

- **Telegram bot for always-on agents**, in the style of open-dots. Link your Telegram chat with a
  one-time code, then message the bot to run tasks in persistent threads (reply to a result to
  continue it, `/new` for a fresh one, `/stop` to interrupt). Pick the provider, model, project and
  access mode with `/provider`, `/model`, `/project` and `/access`. Schedule recurring prompts with
  `/schedule hourly|daily HH:MM|weekdays HH:MM|weekly <day> HH:MM|every 30m <prompt>` in your own
  time zone (`/tz`), and manage them with `/schedules`. Approvals arrive as Allow once / Allow for
  session / Deny buttons showing the exact command or files, and never auto-deny. Scheduled run
  results are sent to the chat. Set it up in Settings → Agents → Telegram. Runs in the engine, so it
  works on a headless server too.

### Fixed

- A local-only install no longer offers to restart into sync ("Sync needs a restart").

## 0.1.0 (2026-10-16)

First release of Godspeed, a fork of [Zeron](https://github.com/zeronsh/zeron) by Wing. The version
was reset from Zeron's 0.2.113. This entry lists what Godspeed adds or changes on top of Zeron.

### Added

- **Muse Code** support through a native driver over MSP (`muse serve`).
- **Access modes and approvals** for every agent: supervised, auto-accept edits (default), auto and
  full access, per chat, with an approval card and an access-mode picker (Cmd+Shift+M cycles).
  Cursor supports auto and full access only; Pi supports full access only.
- **Git actions**: commit, push, pull, sync, create PR and publish branch from the Changes pane and
  the command palette, with generated commit messages and PR descriptions. A "Remove agent credits"
  setting (on by default) strips agent trailers from generated text.
- **Always-on agents** (Settings → Agents): cron, interval, one-shot, webhook and file-watch
  triggers, a memory file injected into each run, and an inbox. Runs skip while the previous run is
  active, and three consecutive failures pause the agent.
- **Chat import** from T3 Code, Claude Code, Codex and Cursor (Settings → Import, `godspeed import`).
  Claude Code and Codex chats continue their original sessions; Cursor chats import as history.
- **Instant search**: Cmd+F find in the open chat and message-text search in the palette, backed by
  a local transcript index.
- **Usage limits**: a chat that hits a provider usage limit shows as Limited with its reset time and
  can resume automatically at the reset. Always-on agent runs wait for the reset instead of failing.
- **Delegation**: a parent chat wakes when a chat it created through the MCP server finishes, fails
  or needs approval.
- **Chat lifecycle**: settle, snooze (presets or a typed time), wake and mark unread from the
  sidebar, Snoozed and Settled shelves, optional auto-settle, and undo with Mod+Z.
- **Worktrees**: an editable branch prefix, a list of unused worktrees in Settings with a delete
  action, and an option to delete a chat's worktree along with the chat.
- Session rows show "Working · Nm", +/- diff stats, live background work and "Waiting on N tasks".
- Shortcuts: Mod+[ and Mod+] for back and forward, Mod+Shift+T to reopen a closed tab,
  Mod+Shift+G for the Changes pane, Mod+Shift+D to settle a chat.
- A confirmation before quitting while agents run in the embedded engine.
- Long pastes attach as a text file; dropped folders and oversized files are referenced by path.
- Headless benchmarks against T3 Code ([BENCHMARKS.md](docs/godspeed/BENCHMARKS.md)).

### Changed

- Rebranded to Godspeed: app name, bundle id `site.shrithan.godspeed`, data directory `~/.godspeed`,
  `GODSPEED_*` environment variables, and a new app icon. Internal crates keep their `zeron-*`
  names so upstream fixes can still be cherry-picked.
- Local-only by default: no baked-in sync service or sign-in client. Sync needs your own `edge/`
  deployment.
- The in-app updater no longer points at Zeron's release feed; auto-update is off for now.

### Fixed

About 45 upstream issues across the agent drivers, engine and UI, including:

- Claude Code: compaction shown in the transcript, full context windows reported as their own
  error, replies that stream no deltas, effort levels clamped to the CLI's maximum, MCP elicitations
  routed to the approval and question cards.
- OpenCode: questions answered through the 2.x form API, background subagents settled, `opencode
  serve` spawned in its own process group, user permission patterns kept in auto mode.
- Engine: agent processes left by an earlier engine are cleaned up at startup, a held queue is
  frozen on Stop before a usage limit lifts, background git never prompts for credentials, terminal
  output queues are bounded.
- Agent CLIs: login-shell PATH capture retries on timeout and supports nushell, terminal escapes are
  stripped from captured output, a single JSON-RPC line from an agent is size-capped, project skills
  are discovered first.
- Sync is never offered on a local-only install.
