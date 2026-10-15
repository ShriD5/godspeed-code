# Godspeed vs T3 Code parity

Audited 2026-10-15 against Godspeed `c0c9b9dd` (the Zeron baseline plus rebrand, before Muse and
always-on agents land) and T3 Code `339179f5`. Source reading only: nothing was built or run for
this audit. ✅ has it (equal or better) · 🟡 partial · ❌ missing · ⛔ out of scope by decision ·
➕ Godspeed-only.

## 1. Agents and providers

| Item | Status | Notes |
|---|---|---|
| Codex (app-server) | ✅ | steer, interrupt, subagents, images, fast tier; no native fork/rollback |
| Claude Code (stream-json) | ✅ | effort incl. ultrathink/ultracode, fast, thinking, 1M context, in-place model switch |
| Cursor | 🟡 | agent questions disabled, no image path |
| Grok Build (ACP) | ✅ | |
| OpenCode 1.x / 2.x | ✅ | |
| Antigravity | ✅ | |
| Pi | 🟡 | no stop-in-place |
| Muse Code | ❌ | in progress on a branch |
| Generic ACP registry (install any agent) | ❌ | only Antigravity's manifest |
| Gemini CLI | ❌ | would come via ACP registry |
| CLI install / update management | ✅ | update policies (notify, auto when idle) beat T3 |
| Login flows | ✅ | Claude PKCE, Codex, Grok device auth, Antigravity, more |
| Managed Codex ChatGPT runtime | 🟡 | wraps CLI login only |
| Skills (`$skill`) | ✅ | |
| ➕ Devin, Hermes agents | ➕ | |
| ➕ Codex realtime voice | ➕ | |

## 2. Model routing and accounts

| Item | Status | Notes |
|---|---|---|
| Per-provider defaults, favorites | ✅ | |
| Custom models | ❌ | |
| Hidden / reordered models | ❌ | |
| Remote model manifest | ❌ | uses live CLI catalogs with curated fallbacks instead |
| Multiple accounts | 🟡 | credential swap into one live CLI store: one account live at a time, no parallel accounts |
| Cross-account continuation | 🟡 | works implicitly after a swap |
| Same-provider model/effort switch mid-thread | ✅ | in place or restart+resume |
| Cross-provider switch mid-thread | ❌ | harness locks once a chat exists |
| Context handoff to new provider | 🟡 | side-chat fork carries the whole transcript, no 16k budget |
| Usage and rate-limit display | ✅ | more providers than T3, composer ring with one-click account switch |
| Usage-limit recovery (resume at reset, snooze) | ❌ | |
| Automatic account/provider fallback | ❌ | T3 lacks it too: planned differentiator |
| Usage and cost page | ❌ | Zeron dropped token accounting on purpose |

## 3. Safety, modes, plans

| Item | Status | Notes |
|---|---|---|
| Access modes (supervised / auto-accept edits / auto / full) | ❌ | **agents run without asking**: Codex `approvalPolicy: never`, Claude auto-allows, ACP auto-picks allow |
| Approval UI (command, file, read, permission) | ❌ | |
| Approval decisions (once / this session / always / decline) | ❌ | |
| Secret request card | ❌ | |
| Plan mode + proposed plan card (Implement / Refine) | ❌ | |
| Todo / plan steps panel | ✅ | |
| Agent questions wizard (1-9 keys, multi-select) | ✅ | |

## 4. Threads: history, fork, queue, recovery

| Item | Status | Notes |
|---|---|---|
| Git checkpoints, rewind, edit-from-here, revert files | ❌ | |
| Fork from a chosen response | 🟡 | forks the whole chat from the latest reply only |
| Merge fork back | ❌ | |
| Queue after active / steer active | ✅ | |
| Queue reorder, edit, remove, promote to steer | ✅ | CRDT-synced across devices (better) |
| Restart-active dispatch mode | 🟡 | |
| Auto-continue after restart (3 tries) | ✅ | same cap as T3 |
| Native subagents as child transcripts | ✅ | |
| Delegation with wake-on-complete | 🟡 | MCP create/wait only, no wake policy |
| Compaction marker in timeline | 🟡 | `/compact` passes through, no event |
| Context window meter | ✅ | |
| Persistence | 🟡 | Loro docs + run journal: sync-friendly, but no transactional outbox like T3 |

## 5. Text generation

| Item | Status | Notes |
|---|---|---|
| Thread titles, configurable model | ✅ | Codex/Claude only |
| Regenerate title | ❌ | |
| Commit messages, PR title/body, branch names, review guides | ❌ | |

## 6. Sidebar

| Item | Status | Notes |
|---|---|---|
| New thread, project filter, grouping modes | ✅ | ➕ group by device |
| Thread search | 🟡 | palette only, titles not message text |
| Lifecycle sections (Working / Snoozed / Settled) | 🟡 | user-named sections + pins instead |
| Status kinds | 🟡 | no pending-approval, plan-ready, limited, unsent-draft |
| Working duration + diff stats on rows | ❌ | |
| Snooze, settle, wake, auto-settle | ❌ | |
| Multi-select, sweep, undo (mod+z) | ❌ | |
| Drop files on a thread row | ❌ | |
| Mark unread, copy branch | ❌ | |
| Hover card on rows | 🟡 | session card for the selected chat |

## 7. Timeline

| Item | Status | Notes |
|---|---|---|
| Streaming smoothness, virtualization | ✅ | better than T3 (incremental parse, paint-only highlight, fade veil) |
| Tool grouping summaries, live tool row | ✅ | |
| Raw command output | ➕ | T3 hides it |
| Subagent / lineage rows, minimap, scroll pill | ✅ | |
| Markdown: GFM, tables, mermaid, code copy, file links | ✅ | |
| GitHub alerts, details, citations, video, image gallery | ❌ | |
| Inline changed-files card per reply | 🟡 | Changes pane has a latest-turn scope |
| Interrupted dividers, compaction rows | ❌ | |
| Find in thread | ❌ | |
| Cite selection into composer | ❌ | |
| Visual replies | 🟡 | ➕ native Luau visuals; no HTML/MCP-App hosting |

## 8. Composer

| Item | Status | Notes |
|---|---|---|
| `/` commands, `$` skills, file `@` mentions | ✅ | |
| `@` thread search | 🟡 | drag from sidebar only |
| `#` pull requests | ❌ | |
| Chips: terminal excerpt, PR, citation | ❌ | image/file/dir/skill/chat chips exist |
| Paste large text as file | ❌ | |
| Attachment limits | 🟡 | 24 MB per file, no count/total caps |
| Model picker, traits picker, favorites | ✅ | |
| Multi-model fan-out to parallel threads | ❌ | |
| mod+1-9 inside model picker | ❌ | |
| Host / repo / branch / worktree pickers | ✅ | |
| Access mode picker | ❌ | |
| Stash, prompt history (ArrowUp) | ❌ | |
| Button states (compact-and-send, implement, disconnected) | 🟡 | send / queue / stop only |
| Draft hero, send-shortcut setting | ✅ | |

## 9. Command palette and shortcuts

| Item | Status | Notes |
|---|---|---|
| Palette with threads, files, content search, commands | ✅ | |
| Projects / environments / directories groups, `>` prefix | 🟡 | tabs instead |
| Palette actions (find, link PR, restart session, copy prompt link, usage) | ❌ | |
| Core shortcuts (sidebar, terminal, palette, new, jump 1-9, model) | ✅ | |
| Back/forward, reopen tab, diff toggle, find, settle/pin, undo, split terminal | ❌ | |
| Keybinding recorder with conflicts | ✅ | refuses conflicts outright |
| Keybindings file with `when` clauses | 🟡 | ~20 fixed actions vs T3's ~60 rules |

## 10. Panels and dev tools

| Item | Status | Notes |
|---|---|---|
| Tabbed right panel, maximize | ✅ | ➕ terminal/subagent/side-chat/PR tabs |
| Close all, rename tab, reopen closed | ❌ | |
| Diff: working tree / branch / latest turn / commit / history scopes | ✅ | ➕ commit + history scopes |
| Diff: split/unified, wrap, fold all, inline review comments | ✅ | |
| Diff: file tree, hide whitespace, find | ❌ | |
| File browser, preview, editor with autosave | ✅ | |
| Rendered HTML / CSV / PDF / audio | ❌ | markdown + images only |
| File picker mod+p, content search case/word/regex | 🟡 | via palette, smart case only |
| Preview browser | 🟡 | native webviews + server discovery; no zoom, viewports, recording, picker, agent control |
| iOS / Android simulators | ❌ | |
| Thread details panel | 🟡 | no linked PRs, lineage, related threads |
| ➕ Git history lane graph, comments on any file | ➕ | |

## 11. Terminal and worktrees

| Item | Status | Notes |
|---|---|---|
| Terminal drawer with tabs, scrollback | ✅ | 10k lines client, 1 MiB server replay |
| Splits | ❌ | |
| Add selection to chat, clickable links | ❌ | |
| Survive engine restart | 🟡 | survives reconnect, not restart |
| New worktree / reuse / current checkout, setup scripts, location | ✅ | |
| Start from origin, naming options | 🟡 | |
| Submodules, cleanup policies, worktree ↔ main handoff | ❌ | |

## 12. Git and pull requests

| Item | Status | Notes |
|---|---|---|
| Commit / push / pull / sync | ❌ | **no git write actions at all** |
| Create PR with generated text | ❌ | |
| Init existing folder, publish repo | ❌ | |
| Branch picker, discard changes | ✅ | |
| Auto pull / fetch interval | ❌ | |
| PR board with filters, sorting, starring | 🟡 | open PRs only, ➕ attention triage |
| PR detail: summary, code, activity, checks | 🟡 | no viewed marks, no AI guide |
| Review submit, line comments, reply/resolve | ❌ | read-only threads |
| Merge, auto-merge, close, draft/ready, delete branch, revert | ❌ | |
| Reviewers, labels, reactions, stacks | ❌ | |
| Checkout PR into worktree | 🟡 | copies the command |
| Agent handoff from PR (review, fix checks, resolve conflicts) | ✅ | |
| GitHub | 🟡 | via `gh`, read-mostly |
| GitLab, Bitbucket, Azure DevOps, Forgejo, GitCafe | ❌ | |

## 13. MCP and automation

| Item | Status | Notes |
|---|---|---|
| MCP server for agents (stdio) | 🟡 | ~17 chat-orchestration tools vs T3's ~90 |
| HTTP MCP endpoint with OAuth for outside agents | ❌ | |
| Worktree / PR / preview / device MCP tools | ❌ | |
| Scheduled tasks, webhooks | ❌ | in progress as always-on agents |

## 14. Settings and appearance

| Item | Status | Notes |
|---|---|---|
| Searchable settings | ❌ | |
| Per-project overrides with provenance | 🟡 | per-device only |
| Notifications (banner, background-only, per-event sounds) | ✅ | |
| Time format, streaming mode, confirmations, hold-to-quit | ❌ | |
| Integrations / Source control / Connections / Storage / Diagnostics / Licenses pages | ❌ | |
| Project actions + `godspeed.json` | ✅ | |
| Archived threads | ✅ | |
| Light/dark/system, 30 built-in themes, independent light/dark picks | ✅ | |
| VS Code theme import | 🟡 | local files with mapping review; no Open VSX search |
| In-app theme editor, eyedropper, export | 🟡 | edit on disk |
| Contrast slider, diff color schemes, font smoothing | ❌ | |
| Chat width, fonts (UI/code/terminal) | ✅ | no separate prompt font |
| ➕ Wallpapers, accent presets, Appshots, local voice dictation | ➕ | |

## 15. Onboarding, desktop, CLI, updates

| Item | Status | Notes |
|---|---|---|
| Welcome wizard, agent setup, history import | ❌ | |
| OS notifications, sounds, toasts | ✅ | Windows banners no-op |
| Dock badge | ❌ | |
| Native menus, window restore, open-thread URL scheme | ✅ | |
| Prompt links (new thread with prompt/repo/model) | ❌ | |
| Default browser, CLI shim install, `app` command | ❌ | |
| Headless engine + background service | ✅ | |
| Auto-update | 🟡 | works, off until a release feed exists; no channels or deltas |

## 16. Remote, mobile, observability

| Item | Status | Notes |
|---|---|---|
| Pairing links / QR, scoped tokens | ❌ | |
| Tailscale, SSH remote backend, WSL | ❌ | |
| Engine reachable beyond localhost | ❌ | |
| Multi-device sync | 🟡 | works with a self-hosted edge, off by default |
| iOS app | 🟡 | native, needs the edge |
| T3 Connect relay, T3 mobile compatibility | ⛔ | |
| SnapShots, cookie import | ⛔ | |
| Logs, run journals | 🟡 | |
| OTLP, diagnostics page, crash reporting, triage | ❌ | |
| Telemetry | ✅ | none collected |
