# Godspeed roadmap

Godspeed is a fork of [zeron](https://github.com/zeronsh/zeron) (MIT). The goal is one native app for
everything AI: the fastest and best-looking coding-agent client, with T3 Code-level depth and
always-on agents built in.

Reference sources (read-only, never run): T3 Code (`pingdotgg/t3code`, MIT), open-dots
(`Anil-matcha/open-dots`, MIT), Muse Code SDK (`@muse-code/sdk`, MIT). Zed's `editor`, `terminal`,
`markdown` and `agent_*` crates are GPL: never copy from them. waku is GPL: ideas only.

## Fork hygiene (do first)

- [ ] Updater: stop reading `edge.zeron.sh/releases`; point at Godspeed releases, off until we publish.
- [ ] Sync edge: default `DEFAULT_EDGE_URL` / `PRODUCTION_EDGE_URL` to none; local-only profile.
- [ ] User-facing rebrand: binary, `.app`, bundle id, data dir, `ZERON_*` env vars, UI strings, icon,
      README, install scripts. Internal `zeron_*` crate names stay for now so upstream merges stay cheap.
- [ ] README credits zeron prominently.

## v0.1 — launch cut

Providers
- [ ] **Muse Code**: native driver over MSP (`muse serve`), modeled on the Pi driver
      (`crates/harness/src/pi`). Types from `@muse-code/sdk` `msp.d.ts`. Modes: ask-me / unrestricted.
      Steering yes, rollback/fork no.
- [x] Claude, Codex (native), Cursor, Grok, OpenCode, Pi, Antigravity, Devin (inherited).
- [ ] Gemini CLI via ACP registry (`gemini --acp`).

T3 depth zeron lacks (audited in PARITY-T3.md)
- [ ] **Access modes + approvals** (supervised / auto-accept edits / auto / full): today every agent runs without asking. Highest priority.
- [ ] **Git write actions**: commit, push, pull, sync, create PR with generated text (none exist today).
- [ ] Plan mode with Implement / Refine on the proposed plan.
- [ ] Checkpoints as hidden git refs + rewind + "edit from here" (T3: `CheckpointService.ts`).
- [ ] Fork thread from a response, merge back to source.
- [ ] Queue: reorder, edit, promote-to-steer.
- [ ] Model routing: per-account instances, mid-thread switch with context handoff
      (T3 caps handoff at 16k tokens of collapsed items), and **automatic fallback** when an account
      hits its usage limit (T3 only snoozes; we continue on the next account/provider).
- [ ] Text generation on a cheap model: titles, commit messages, PR bodies, branch names.
- [ ] Command palette (`mod+k`) with actions/projects/threads; keybindings file with `when` clauses.
- [ ] Access modes: supervised / auto-accept edits / auto / full access.

Always-on agents (open-dots, done properly)
- [ ] Agents: name, provider/model, workspace, memory file injected each run.
- [ ] Triggers: cron (misfire grace 15 min, skip if still running, auto-pause after 3 failures),
      webhooks (signed URL, payload templated into prompt), file watch, GitHub events.
- [ ] Inbox of results: unread/archived, links to the thread and diff, needs-approval items.
- [ ] Approvals showing the exact command/path; rules (bucket → connector → tool) with a revoke page;
      unanswered approvals queue in the inbox instead of auto-denying.
- [ ] Channels: native notifications, Telegram (Allow once / Always / Deny buttons).
- [ ] Runs on the headless engine so a VPS keeps agents alive when the laptop sleeps.

Design pass
- [ ] Animate panels at 120fps (T3 ships panel animation at 0ms because DOM layout is expensive).
- [ ] Show raw tool output (T3 hides command stdout).
- [ ] Local thread search index (T3 needs a server round trip and 2+ chars).

## v0.2

GitHub PRs and git actions (commit / push / create PR with generated text), usage and cost page,
file editor, project content search, MCP server for outside agents, scheduled-task webhooks UI,
Sparkle auto-update + notarized DMG.

## v0.3

Web and phone client against the headless engine, other git hosts, agent-driven preview browser.

## Not planned

SnapShots, browser cookie import, T3 Connect relay and T3 mobile app wire compatibility.
