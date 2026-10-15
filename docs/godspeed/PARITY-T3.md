# Godspeed vs T3 Code parity

Re-audited 2026-10-15 against Godspeed `856c94d3` and T3 Code `339179f5`. The previous version was
written at `c0c9b9dd`; 102 commits have landed since (access modes and approvals, git write actions,
always-on agents, Muse Code, chat import, ~45 upstream issue fixes). Source reading only: nothing was
built or run. Every row was re-checked against the current code, not carried over.

Legend: ✅ has it (equal or better) · 🟡 partial · ❌ missing · ⛔ out of scope by decision · ➕ Godspeed-only.

T3 feature list built from `docs/user/*.md`, `packages/contracts/src/{settings,rpc}.ts`,
`packages/shared/src/keybindings.ts`, `apps/web/src/components` and `apps/server/src`.

## Summary

| Status | Count |
|---|---:|
| ✅ has it | 104 |
| 🟡 partial | 56 |
| ❌ missing | 83 |
| ⛔ out of scope | 11 |
| ➕ Godspeed-only (not counted) | 6 |

**Parity: 54%** = (✅ + ½·🟡) over the 243 in-scope T3 items (✅ + 🟡 + ❌). ⛔ and ➕ rows are
excluded from both sides. The previous version scored 48% over 154 in-scope rows (decided-out items
were counted as ❌ then). This list is broader and finer-grained, so the two percentages are not
directly comparable; the movers are listed below.

### Changes since the previous version (`c0c9b9dd`)

| Moved | From → to | Evidence |
|---|---|---|
| Muse Code harness | ❌ → ✅ | `crates/harness/src/muse/` |
| Access mode picker, approvals on every harness except Cursor/Pi | ❌ → ✅ | `proto/src/access.rs`, `harness/src/approval.rs` |
| Git writes and generated commit/PR/branch text | ❌/🟡 → ✅ | `engine/src/git_actions.rs`, `text_generation.rs` |
| Always-on agents: cron, interval, one-shot, webhook, file watch | ❌ → ✅/🟡 | `engine/src/agents.rs` |
| Chat import (T3, Claude, Codex, Cursor) | ❌ → ✅ | `engine/src/chat_import/` |
| No-project threads | new row → ✅ | `proto/src/projectless.rs` |
| Large paste as file, folders and big files by path | ❌ → ✅ | `ui/src/attachments.rs`, `d37cf2de` |
| Compaction and background-work visibility | 🟡/❌ → ✅ | `68870680`, `2a532226` |
| Palette actions (git, archive) | ❌ → 🟡 | `shell/command_palette.rs` |
| Quit confirmation | ❌ → 🟡 | `784ef5df` |
| Worktree creation lock, dirty-removal guard, branch prefix | ❌ → 🟡/✅ | `898055b6` |
| Process hygiene (orphan reaping, bounded queues, PATH retry) | not a T3 row; ➕ | `ISSUES-TRIAGE.md` batches 1-3 |
| New T3 rows the old list missed | n/a | provider instances, goals, linked PRs and PR watch, review guides, devices hub, MCP Apps, secret requests, stash, background start, question attachments, per-project provider switch |

## 1. Agents and providers

| Item | Status | Notes |
|---|---|---|
| Codex (app-server) | ✅ | steer, interrupt, subagents, images, fast tier, user `config.toml` respected (`crates/harness/src/codex/`) |
| Claude Code (stream-json) | ✅ | effort clamped to `maxEffortLevel`, 1M context, compaction shown, MCP elicitation routed to cards (`harness/src/claude/`) |
| Cursor | 🟡 | SDK shim: agent questions disabled, no image attachments; no supervised/auto-accept (SDK has no approval channel, T3 has the same limit) |
| Grok Build (ACP) | ✅ | `harness/src/acp/` |
| OpenCode 1.x / 2.x | ✅ | owned process group, background subagents settle, 2.x questions via form API |
| Antigravity | ✅ | pinned archive, scratch dirs reclaimed |
| Pi | 🟡 | extension dialogs work; refused outside full access (T3 offers supervised and auto-accept through Pi's tool hook) |
| Muse Code | ✅ | native MSP driver, modes map to Muse profiles (`harness/src/muse/`) |
| Generic ACP registry (search/install any agent) | ❌ | only Antigravity's manifest; `engine/src/harness_updates.rs` |
| Gemini CLI, Kimi, other registry agents | ❌ | would arrive with the registry |
| CLI install, update, rollback management | ✅ | update policies notify / auto-when-idle beat T3 (`engine/src/harness_updates.rs`) |
| Login flows (Claude PKCE, Codex, Grok, Antigravity, Cursor) | ✅ | `engine/src/agent_accounts/oauth.rs` |
| Managed ChatGPT runtime (no CLI needed) | 🟡 | wraps the CLI login only |
| Skills `$`, plugin skills, project-first discovery budget | ✅ | `harness/src/skills.rs` |
| Provider slash commands pass through (`/compact`, `/feedback`) | ✅ | `LIST_COMMANDS`; no T3-style client commands |
| Native subagents as child transcripts | ✅ | Codex, Claude, Cursor, ACP, OpenCode |
| Per-provider instances (binary path, env vars, launch args, config dir) | ❌ | one instance per harness; PATH from login shell (`harness/src/shell_env.rs`) |
| Provider on/off per project | ❌ | per-device toggle only (`settings/harnesses.rs`) |
| Provider update notifications and dismissals | ✅ | Settings → Providers, `shell/harness_updates.rs` |

## 2. Model routing and accounts

| Item | Status | Notes |
|---|---|---|
| Per-provider defaults, favorites, remembered last model | ✅ | `settings/composer.rs` (`composer-defaults.json`) |
| Compact model picker with effort / fast / context traits | ✅ | `ui/src/pickers/compact.rs`, `client/src/catalog.rs` |
| Per-project model default | ❌ | one global remembered pick |
| Custom models (unlisted name + options) | ❌ | live CLI catalogs only |
| Hide or reorder models | ❌ | also lets delegated agents use hidden models in T3 (#15992) |
| Remote model manifest | ❌ | live CLI catalogs with curated fallbacks instead |
| `mod+1-9` and `mod+shift+up/down` inside the picker | ❌ | |
| Multiple accounts | 🟡 | credential swap into one live CLI store: one account live at a time (`engine/src/agent_accounts.rs`) |
| Switch account from the thread | 🟡 | composer usage ring switches globally; T3 picks an account per thread in parallel |
| Same-provider model/effort switch mid-thread | ✅ | in place or restart plus resume |
| Usage and rate-limit display | ✅ | composer ring, more providers than T3 (`ui/src/account_usage.rs`, `agent_accounts/usage.rs`) |
| Usage windows survive a failed probe | ✅ | `a0c5eb9f` |
| Limited status, Resume at reset, Snooze until reset, auto-resume | ✅ | per-driver detection with reset times, usage-window fallback, held sends, engine timer that survives restarts and sleep (`engine/src/limit_recovery.rs`, `harness/src/limits.rs`); snooze marks rather than hides (no sidebar snooze yet); same chat, same account only |
| `/usage-limits` in the composer | ❌ | |
| Pooled limits across accounts, CLIProxyAPI hubs, "Use reset" credits | ❌ | |

## 3. Safety, modes, approvals

| Item | Status | Notes |
|---|---|---|
| Access modes: supervised / auto-accept edits / auto / full | ✅ | per chat, default in Settings, `⌘⇧M` cycles (`proto/src/access.rs`); Cursor auto+full only, Pi full only |
| Mode picker in the composer | ✅ | access chip, hides in narrow composers |
| Approval card (command, cwd, paths, diff, reason) | ✅ | `ui/src/composer.rs`, `proto::ApprovalRequest`; long commands wrap |
| Provider-native decisions (once / session / always / decline) | ✅ | Codex, Claude, OpenCode, ACP, Muse (`harness/src/approval.rs`) |
| "Needs approval" sidebar state, sound, banner | ✅ | `shell.rs` row status |
| Approve from phone or another device | ✅ | answered through the question sheet |
| Orchestrator answers approvals within its own mode | ✅ | `mcp respond_to_input` (`62032b47`) |
| Claude silent denials end as needs-attention | ✅ | `ecc3c60f` |
| Secret request card | ❌ | |
| Plan mode, proposed plan card (Implement / Refine) | ⛔ | decided out |
| Todo / plan steps panel | ✅ | `ui/src/todo_panel.rs` |
| Agent questions wizard (digit keys, multi-select, option previews) | ✅ | `composer.rs`; typed answer survives cancel |
| Files and images in question answers | ❌ | |
| Dismiss an async question without answering | 🟡 | cancel works; no Codex async-question distinction |

## 4. Threads: history, queue, recovery

| Item | Status | Notes |
|---|---|---|
| Git checkpoints, rewind, edit-from-here, revert files | ⛔ | decided out |
| Fork from a chosen response | 🟡 | side chat forks the whole chat from the latest reply (`FORK_SIDE_CHAT`) |
| Merge a fork back | ❌ | |
| Queue after active / steer active | ✅ | `ui/src/queue.rs` |
| Queue reorder, edit, remove, promote to steer | ✅ | CRDT-synced, phone sees the same queue (better) |
| Steer / queue default setting and opposite-action key | 🟡 | text steers by default, attachments offer interrupt; no setting |
| Queue survives restart, held with Resume | 🟡 | queue is doc-backed and survives; no explicit held state or Resume control |
| Auto-continue after restart (3 tries) | ✅ | same cap as T3 |
| Delegation with wake-on-complete and approvals bubbled to parent | 🟡 | MCP `create_chat` + `wait_for_turn` only (T3 #15082, Zeron #768) |
| `/goal` row above the composer | 🟡 | passes through as a provider command; no goal row or pause/resume |
| Compaction notes in the timeline | ✅ | "Context compacted." for Claude and Codex (`68870680`); no divider row |
| Compact-on-return chip (cache expired) and context meter menu | 🟡 | context ring yes, no compact action or chip |
| Context window meter | ✅ | `ui/src/context_usage.rs` |
| Live background work on the row ("waiting on N tasks") | ✅ | `2a532226`; done chime held meanwhile |
| Persistence | 🟡 | Loro docs + run journal, sync-friendly; no transactional outbox like T3 |
| Interrupt / Escape stops the agent | ✅ | `escape_stops_active_agent` setting |

## 5. Text generation

| Item | Status | Notes |
|---|---|---|
| Thread titles with a configurable model | 🟡 | Codex and Claude only (`supports_titles`), cheapest-model heuristic |
| Regenerate title | ❌ | manual rename only |
| Commit message, PR title/body, branch name generation | ✅ | repo-style aware, PR template aware, credits stripped (`engine/src/text_generation.rs`) |
| Writing style, writer model, repository conventions setting | 🟡 | one shared model (Settings → Thread naming); no per-task model |
| Review guides (beta) | ❌ | |
| Worktree branch prefix, semantic prefix, custom naming prompt | 🟡 | static prefix exists in engine (`godspeed/`), no UI; generation RPC not wired to worktree naming |
| Generation failure surfaced, not silent | ✅ | `GenerationUnavailable` error in git actions |

## 6. Sidebar and thread lifecycle

| Item | Status | Notes |
|---|---|---|
| New thread, project filter, grouping, sorting | ✅ | ➕ group by device, compact rows, show/hide metadata |
| New thread without a project (scratch folder) | ✅ | `proto/src/projectless.rs`, "Don't work in a project" |
| Thread search | 🟡 | palette scores title / project / branch / device / PR, not message text (`shell/thread_search.rs`) |
| Pin | ✅ | `shell/sidebar_pins.rs`, synced |
| Lifecycle sections (Working / Snoozed / Settled) | 🟡 | user-named sections plus archive instead |
| Status labels | 🟡 | Working, Input, Needs approval, Failed, Limited, Done, Queued, subagent pill; no unsent-draft |
| Working duration and diff stats on rows | ❌ | `working_since_ms` exists in `client/src/session`, never rendered |
| Settle, snooze, wake, auto-settle (inactivity, merged PR) | ❌ | |
| Multi-select, drag-to-settle, undo toast with `mod+z` | ❌ | |
| Drop files on a thread row | ❌ | drop works on the open chat only (`shell/chat_dropzone.rs`) |
| Mark unread, copy thread reference or branch | ❌ | |
| Hover card on rows | 🟡 | session card for the selected chat (`shell/session_info.rs`) |
| Archive, archived page, delete | ✅ | `settings/archived.rs`, `shell.rs::delete_chat`; no "delete the worktree too?" prompt |
| Discard unsent draft from sidebar with undo | ❌ | |
| Linked pull requests per thread (badge, link/unlink, watch) | 🟡 | branch-detected PR badge only; no manual link, no stacks, no watch |
| Environment/device icons and project icons | ✅ | `shell/project_icon.rs`, device badges |
| Per-chat scroll position restore | ✅ | `ui/src/history.rs` anchors, diff scroll per chat |

## 7. Timeline

| Item | Status | Notes |
|---|---|---|
| Streaming smoothness, virtualization | ✅ | veil fade, incremental parse, paint-only highlight; `docs/performance-*.md` |
| Tool grouping summaries, live tool row, shimmer only while live | ✅ | `ui/src/transcript.rs` |
| Raw command output and tool result bodies | ➕ | T3 hides them (listed under Godspeed-only) |
| Subagent / lineage rows, message rail minimap | ✅ | `ui/src/rail.rs` |
| Markdown: GFM, tables, mermaid, code copy, file links, Codex citations | ✅ | `crates/markdown`, `ui/src/markdown/` |
| GitHub alerts, details blocks, video, image gallery, copyable messages | ❌ | |
| Inline changed-files card per reply | 🟡 | Changes pane has a latest-turn scope; no inline card |
| Interrupted divider | ❌ | |
| Find in thread (`mod+f`) | ❌ | |
| Cite selection into composer | ❌ | |
| Visual replies (agent-built pages) | ✅ | native Luau visuals (`ui/src/visuals.rs`) instead of sandboxed HTML; works on any harness |
| MCP Apps (interactive tool results) | ❌ | |
| Image viewer, media lightbox | ✅ | `ui/src/image_viewer.rs` |

## 8. Composer

| Item | Status | Notes |
|---|---|---|
| `/` commands, `$` skills, file `@` mentions | ✅ | `ui/src/composer.rs` |
| `@` thread mentions (agent reads on demand) | ✅ | `proto/src/chat_mentions.rs`, chip plus sidebar drag |
| `#` pull request chips and search | ❌ | |
| Chips: image, file, folder, skill, command, chat | ✅ | `composer/chip.rs` |
| Chips: terminal excerpt, PR, review comment, citation | 🟡 | review comments stage into the prompt (`comments.rs`); no terminal excerpt, PR or citation chip |
| Source-aware markdown, lists, task lists, quotes, code fences | ✅ | `composer_markdown.rs` |
| Large paste becomes a text-file chip | ✅ | `attachments.rs::stage_pasted_text` (`adccb0c7`); paste never makes mentions |
| Folders and oversized files referenced by path | ✅ | `d37cf2de`, better than T3's size refusal |
| Attachment limits | 🟡 | 24 MB per file; no count or total caps, no HEIC convert or resize |
| Model picker, traits, favorites | ✅ | |
| Multi-model fan-out to parallel threads | ❌ | |
| Host / repo / branch / worktree pickers | ✅ | `pickers.rs` |
| Access mode picker | ✅ | |
| Prompt stash (`mod+s`) | ❌ | |
| Prompt history recall (ArrowUp) | ❌ | |
| Draft hero, send shortcut (Enter / mod+Enter) | 🟡 | two of T3's three send modes |
| Start in background (`mod+alt+enter`) / send and new thread | ❌ | |
| Button states (compact-and-send, resume, disconnected) | 🟡 | send / queue / stop / retry-undelivered |
| Composer undo and redo grouping | ✅ | |
| Voice input in the composer | ✅ | ➕ local Parakeet dictation (`crates/voice`) |
| Drag or paste images, drag folders | ✅ | |

## 9. Command palette and shortcuts

| Item | Status | Notes |
|---|---|---|
| Palette: threads, recent files, files, in-file content, commands | ✅ | `shell/command_palette.rs`, tabs All / Threads / Commands / Files |
| Palette actions | 🟡 | new chat/project, settings, theme, archive, git commit/push/pull/PR; no find, link PR, restart session, copy link, usage |
| Projects / environments / directories groups, `>` actions prefix | ❌ | tabs instead |
| Core shortcuts: sidebar, terminal, new, jump 1-9, model, access mode | ✅ | `ShortcutId` in `settings.rs` |
| Rebindable commands | 🟡 | 16 plus 9 jump slots, vs ~60 T3 commands |
| Keybinding recorder with conflict handling | ✅ | refuses conflicts outright |
| Keybindings file with `when` clauses | ❌ | settings JSON only, no `when` |
| Back/forward, reopen closed tab, diff toggle, find, settle/pin, steer-queued | ❌ | |
| Preview and terminal scoped shortcuts (split, zoom, focus URL) | ❌ | `browser_reload` only |
| Open in editor shortcut | ❌ | |
| Hold-to-quit and confirm quit | 🟡 | confirms only while agents run (`784ef5df`); no hold or double-press modes |

## 10. Panels, diff, files, search

| Item | Status | Notes |
|---|---|---|
| Tabbed right panel with maximize | ✅ | ➕ terminal, subagent, side-chat and PR tabs |
| Close all, rename tab, reopen closed view | ❌ | |
| Diff scopes: working tree / branch / latest turn | ✅ | `ui/src/changes.rs` |
| Diff scopes: commit and history | ➕ | `ui/src/history.rs` lane graph |
| Split / unified, wrap, fold per file, inline review comments | ✅ | `changes.rs`, `comment_ui.rs` |
| Diff file tree | 🟡 | PR Code tab has one (`pull_request_code.rs`); Changes pane does not |
| Hide whitespace, find in diff (`mod+f`) | ❌ | |
| File browser, preview, editor with autosave | ✅ | `ui/src/files/`, `settings/files.rs` |
| Rendered HTML / CSV / PDF / audio | ❌ | markdown and images only |
| File picker `mod+p`, project content search | 🟡 | via palette; smart-case only, no regex or whole-word toggle |
| Files outside the workspace (read-only) | ✅ | file links open any path |
| Thread details panel | 🟡 | session card; no linked PRs, lineage or related threads |
| Open in external editor (VS Code, Cursor, Zed, JetBrains) | 🟡 | "Open in default editor" only (`files/preview.rs`) |
| Panel animation | ✅ | animated by default (`shell/panel_layout.rs` eased tweens), Reduce motion respected; no duration slider |

## 11. Preview browser and devices

| Item | Status | Notes |
|---|---|---|
| Embedded browser tabs, profiles | 🟡 | native webviews (macOS, Linux helper, Windows), per-user profile; no incognito |
| Dev-server discovery and stable names | ✅ | `crates/preview`, `docs/preview-networking.md` |
| Zoom, viewports, appearance override, history, favicons | ❌ | |
| Agent-driven browser (`preview_*` tools), element picker, annotations | ❌ | |
| Recording, screenshots, mini player | ❌ | |
| Link target setting (in-app vs system browser) | ✅ | `open_web_links_in_zeron` |
| iOS Simulator / Android Emulator panel, agent device tools | ❌ | |
| ➕ Cross-device project preview over peer streams | ➕ | `crates/preview` (needs edge signaling) |

## 12. Terminal and worktrees

| Item | Status | Notes |
|---|---|---|
| Terminal drawer, tabs, drag-reorder, scrollback | ✅ | `ui/src/terminal/`, 10k lines client, 1 MiB replay |
| Terminal splits | ❌ | |
| Add selection to chat, clickable links | ❌ | |
| DA/DSR replies, bounded output queue | ✅ | alacritty `PtyWrite`, `1458f751`; OSC 10/11 colour queries unanswered |
| Survive engine restart | 🟡 | survives reconnect, not restart |
| New worktree / reuse / current checkout, location setting | ✅ | `settings/worktrees.rs`, `repos.rs`; creation lock per repo |
| Setup scripts on worktree create, project actions | ✅ | `engine/src/project_actions.rs` |
| Run on settle script | ❌ | no settle concept |
| Start from origin, base-branch picker, naming options | 🟡 | base picker yes; no origin toggle or naming UI |
| Dirty-removal guard, orphan listing | 🟡 | engine guards dirty removal and lists orphans (`898055b6`); no UI to review or delete them |
| Submodule modes | ❌ | |
| Cleanup policies (inactive days, merged, settled), Delete now | ❌ | orphan sweep only |
| Worktree to main handoff, "new thread in this worktree" | 🟡 | reuse picker; no handoff |

## 13. Git and pull requests

| Item | Status | Notes |
|---|---|---|
| Commit (all or chosen files) with generated message | ✅ | `engine/src/git_actions.rs`, `ui/src/git_actions.rs` |
| Push, pull (ff-only), sync, never force | ✅ | Changes pane strip plus palette |
| Create PR with generated text, template, draft, base picker | ✅ | `gh pr create` |
| Init folder, publish repository | ✅ | `gh repo create --source . --push` |
| Branch picker, switch ref, discard changes | ✅ | |
| Live branch label, SSH-alias and non-origin remotes | ✅ | `7f7e652e`, `5f8bfd19` |
| Background git never prompts for credentials | ✅ | `15a335d9` |
| Auto pull / fetch interval | ❌ | manual `FETCH_ALL` only |
| Clone repository with progress toast, create new project | ✅ | `CLONE_REPO`, `CREATE_REPO` |
| PR board with filters, sorting, starring | 🟡 | Authored / All / Reviewing, stars, attention triage; open PRs only |
| PR detail: summary, code tab with tree, activity, checks | ✅ | `pull_request_detail.rs`, `pull_request_code.rs` |
| Viewed-file marks | ❌ | |
| Submit review, line comments, reply and resolve threads | 🟡 | post comment only, threads read-only |
| Merge, auto-merge, close, draft/ready, delete branch, revert PR | ❌ | |
| Reviewers, labels, reactions, edit title/body | ❌ | |
| Checkout PR into a worktree | 🟡 | copies `gh pr checkout` |
| Agent handoff from PR (review, fix checks, conflicts) | ✅ | `pull_request_handoff.rs` |
| Stacks (merge/rebase stack) | ❌ | |
| Review guides | ❌ | |
| Remove agent credits when merging | 🟡 | strips from generated and typed commit messages; not applied to GitHub merges |
| GitHub | 🟡 | via `gh`: create, list, detail, comment, publish |
| GitLab, Bitbucket, Azure DevOps, Forgejo, GitCafe | ❌ | |
| GitHub token without `gh` | ❌ | `gh` only |
| Source Control settings page | ❌ | |

## 14. MCP, automation, import

| Item | Status | Notes |
|---|---|---|
| MCP server for agents (stdio) | 🟡 | 16 chat-orchestration tools plus `show_visual` (`crates/mcp/src/tools.rs`); T3 has 40+ across 9 toolkits |
| HTTP MCP endpoint with OAuth for outside agents | ❌ | engine now serves plain HTTP on the IPC listener (`e9be6aa6`), unused for MCP |
| Worktree / PR / preview / device / file MCP tools | ❌ | |
| Scheduled tasks: cron, interval, one-shot | ✅ | always-on agents, misfire grace, skip-if-running, auto-pause (`engine/src/agents.rs`, `agents/schedule.rs`) |
| Webhook tasks with payload placeholders | 🟡 | `{{body.x}}`, `{{headers.x}}`; no `{{query}}`, no signature check, no deliveries log |
| File-watch trigger | ➕ | `AgentTriggerKind::FileWatch` |
| Agent memory file, inbox with unread / needs-approval | ➕ | `MEMORY.md`, `settings/agents.rs` |
| Run now, rotate webhook URL, pause | ✅ | |
| Public webhook URL (managed tunnel) | ❌ | needs a reachable engine or your own edge |
| Offline-held webhooks | ❌ | |
| Orphaned runs reaped, copied DB paused | ✅ | `8eb86665`, `758fd206` |
| Workflow scripts | ❌ | T3 `getWorkflowScript` / `stopWorkflow` |
| Chat import from T3, Claude Code, Codex, Cursor | ✅ | `engine/src/chat_import/`, Settings → Import, `godspeed import`; resumes the native CLI session |
| Import prompt on first launch | 🟡 | dismissible prompt; not a wizard |
| ACP session management (list/import/delete provider sessions) | ❌ | |

## 15. Settings and appearance

| Item | Status | Notes |
|---|---|---|
| Searchable settings | ❌ | 12 pages, no search |
| Per-project overrides with provenance | ❌ | per-device only |
| Notifications: banner, background-only, per-event sounds | ✅ | `settings/notifications.rs`; chime held while background work live |
| Time format, streaming mode, timestamp style | ❌ | |
| Confirmation toggles (archive, delete, unpin, quit) | 🟡 | quit confirm only |
| Connections / Source Control / Storage / Diagnostics / Integrations pages | ❌ | |
| Open-source licenses page | 🟡 | `THIRD_PARTY_NOTICES.md` in repo, no in-app page |
| Project actions and `godspeed.json` | ✅ | |
| Archived threads page | ✅ | |
| Project icon / monogram picker | 🟡 | auto icon and monogram; no picker |
| Light / dark / system, built-in themes, independent light and dark picks | ✅ | `crates/theme` |
| VS Code theme import | 🟡 | local files, mapping review; no Open VSX search |
| In-app theme editor, picker eyedropper, JSON export | 🟡 | edit on disk |
| Contrast slider, diff color schemes, font smoothing | ❌ | |
| Fonts: UI, code, terminal; sizes | ✅ | no separate prompt font |
| Chat width | ✅ | |
| Theme from server (`t3 theme set`) | ❌ | |
| Launch at login, menu-bar item, keep awake while working | ❌ | |
| ➕ Wallpapers, glass, accent presets, Appshots | ➕ | `settings/appearance.rs`, `appshots.rs` |

## 16. Onboarding, desktop, CLI, updates

| Item | Status | Notes |
|---|---|---|
| Welcome wizard: connect computers, check agents, import projects | ❌ | |
| Check/install agents from setup | 🟡 | per-provider install in Settings → Providers |
| OS notifications, sounds, toasts | ✅ | Windows banners are no-ops |
| Dock badge, taskbar progress | ❌ | |
| Native menus, window restore, open-thread URL | ✅ | `app_menus.rs`, `godspeed <url>` |
| Prompt links (new thread with prompt/repo/model) | ❌ | |
| Default browser, CLI shim install, `app` command | ❌ | |
| Headless engine plus background service | ✅ | `godspeed headless`, `daemon install` |
| CLI: status, update, import, mcp | ✅ | `apps/godspeed/src/main.rs` |
| CLI: project add/remove, pair, serve, auth tokens, theme, trace, browser | ❌ | |
| Auto-update | 🟡 | download, verify, swap, restart; feed not published yet; no channels |
| Desktop: macOS, Windows, Linux | ✅ | Linux and Windows less polished (see issues triage) |
| Android app | ❌ | Rust core planned (`docs/mobile-rewrite.md`) |

## 17. Remote access, mobile, observability

| Item | Status | Notes |
|---|---|---|
| Pairing links / QR, scoped tokens | ❌ | sign-in plus edge rooms instead |
| Tailscale, SSH remote backend, WSL | ❌ | |
| Engine reachable beyond localhost without a hosted edge | ❌ | |
| Multi-device sync | 🟡 | works with a self-hosted edge (`edge/`), off by default |
| iOS app | 🟡 | native UIKit over Rust core (`apps/ios`), needs the edge; no push |
| Mobile voice input | ✅ | `crates/mobile/src/client_ffi/voice.rs`, Live Activity target |
| Connection status, slow-request toast | ✅ | `client/src/connectivity.rs` |
| Logs, run journals | 🟡 | tracing + `run_journal.rs`; no log viewer or retention policy |
| OTLP, trace summaries, resource telemetry, process signal actions | ❌ | |
| Diagnostics page, crash reporting | ❌ | panic hook only |
| Telemetry | ✅ | none collected (T3 #4123 complaint) |
| Chat lag fixes, bounded queues, scratch reclaim | ✅ | `docs/godspeed/ISSUES-TRIAGE.md` batches 1-3 |

## 18. Out of scope by decision

| Item | Status | Notes |
|---|---|---|
| Plan mode and proposed plan card | ⛔ | |
| Rewind, checkpoints, edit-from-here, revert files | ⛔ | |
| Automatic account/provider fallback and load balancing | ⛔ | |
| Mid-thread provider switching and portable handoff | ⛔ | prompt cache cost |
| T3 Connect relay, T3 mobile compatibility | ⛔ | |
| SnapShots | ⛔ | |
| Browser cookie import | ⛔ | |
| Usage and cost page | ⛔ | Zeron dropped token accounting |
| Telegram channel | ⛔ | |

## 19. Godspeed-only (not counted)

Raw tool output in the timeline · native Luau visuals · Codex realtime voice and local Parakeet
dictation · Appshots · wallpapers, glass and accent presets · git history lane graph · comments on any
file · group by device and user-named sidebar sections · always-on agents with file-watch trigger,
memory and inbox · four-source chat import that resumes the native CLI session · Devin and Hermes
harnesses · harness update policies · cross-device project preview · CRDT queue shared with the
phone · zero telemetry.

## Biggest remaining gaps (by user impact)

1. **PR review and merge**: submit review, line comments, reply/resolve, merge and auto-merge, draft/ready, viewed marks. Today the PR board is read-only plus comment and agent handoff.
2. ~~**Usage-limit recovery**~~: done (Limited status, Resume at reset, snooze, auto-resume, always-on agents wait for the reset). Left: hiding snoozed chats from Active, `/usage-limits`, account fallback.
3. **Thread lifecycle**: settle, snooze, auto-settle on merged PR or inactivity, multi-select with undo, working duration and diff stats on rows. The sidebar is the surface users look at all day.
4. **Find in thread and message-text thread search** (T3 has both, search starts at two characters).
5. **Delegation that wakes the parent** (child finished, blocked or needs approval) plus a `/goal` row. Multi-agent orchestration is the main use of the MCP server.
6. **Per-project settings, per-project model default, custom/hidden models, provider instances** (binary path, env, launch args, parallel accounts). Blocks OpenRouter, proxy and enterprise setups.
7. **Agent-driven preview browser** (click, type, snapshot, record) and device simulators. T3's loop of "agent checks its own UI work" has no equivalent.
8. **Remote access without the edge**: pairing links, Tailscale, SSH backends; the headless engine is only reachable through the self-hosted edge.
9. **Onboarding**: welcome wizard, agent check, project import. First run is an empty sidebar plus one banner.
10. **Composer ergonomics**: prompt recall (ArrowUp), stash, start-in-background, multi-model fan-out, `#` PR chips, terminal-excerpt and citation chips.
11. **Keybindings**: 16 rebindable actions, no file, no `when`; missing back/forward, reopen tab, find, settle, split terminal.
12. **Outside-agent MCP**: HTTP endpoint with OAuth, plus worktree/PR/file tools; today it is a stdio chat orchestrator.
13. **Settings structure**: search, per-project overrides, Source Control, Storage, Diagnostics pages, project icon picker.
14. **Worktree lifecycle**: cleanup policies (age, merged, settled), submodule modes, origin toggle, run-on-settle script.
15. **Other git hosts and token auth** (GitLab first), auto-fetch/pull, stacks, review guides.

## Where Godspeed is or can be better than T3

### Already better (with proof)

| Area | Evidence |
|---|---|
| Native rendering, idle cost | GPUI app, no browser engine. `docs/performance-resource-usage.md`: ~206 MiB idle RSS, idle CPU 27% to 7.5% after optimisation (Linux software Vulkan, not Metal). T3's tracker has live reports of composer lag (#5626), server memory creep (#5410), animation polling CPU (#4693), timeline battery drain (#16405) |
| Panels animate by default | eased panel tweens (`shell/panel_layout.rs`); T3 defaults panel animation to 0 ms because width and height transitions cost layout work on every frame (`panelAnimationDurationMs` comment in `contracts/src/settings.ts`) |
| Command output visible | raw stdout/stderr in the timeline; T3 shows summaries only (`docs/user/activity-log.md`) |
| Queue shared with the phone | queue rows live on the session doc with edit leases, so phone and desktop reorder and edit the same queue (`ui/src/queue.rs`, `doc/src/queue.rs`) |
| Approvals everywhere | one card across Codex, Claude, OpenCode, ACP, Muse, and answerable from the phone or an orchestrator within its own mode |
| Chat import | four sources, resumes the original CLI session by native id (`chat_import/mod.rs`); T3 imports 200 messages of Claude/Codex without tool activity |
| Always-on agents | headless engine, misfire grace, backlog collapse, skip-if-running, auto-pause after 3 failures, per-agent memory file, run inbox, file-watch trigger; T3 scheduled tasks have no memory or inbox |
| Large paste and oversized files | folders and files past the cap go in by path (`d37cf2de`); T3 refuses past its limits |
| Harness updates | notify or auto-when-idle policy per harness |
| Privacy | zero telemetry; T3 users filed #4123 |
| Visuals | agent-written Luau drawn with native elements, theme-exact, works for every harness without hosting a webview |
| Local thread search | client-side scoring, no server round trip (T3 needs the server and two characters) |
| Process hygiene | agent process groups recorded and reaped at engine start, scratch dirs reclaimed, bounded subscriber queues, JSON-RPC line cap |

### Opportunities to clearly beat T3

| # | What | Why it beats T3 | Size | Files |
|---|---|---|---|---|
| 1 | **Speed receipts**: reproducible turn-latency and resource benchmark against the same CLI under T3 and Godspeed, published in the repo | T3's most-reacted perf issue is "20+ min vs 3m24s for the same Codex task" (#695, 31 reactions); a measured thin-client number is a headline | S | `docs/performance*.md`, `crates/ui/examples/macos-resource-profile.rs` |
| 2 | **Resume at reset** reusing the always-on scheduler's one-shot trigger | T3 ships this; here it is nearly free: next-fire persistence, misfire grace and skip-if-running already exist, and the engine is headless-capable | M | `engine/src/agents/schedule.rs`, `engine/src/sessions.rs`, `agent_accounts/usage.rs`, `ui/src/composer.rs` |
| 3 | **Delegation that wakes the parent** and bubbles child approvals up | T3 #15082 and Zeron #768 are open; MCP already has `wait_for_turn`, `respond_to_input` | M | `crates/mcp/src/tools.rs`, `engine/src/sessions.rs`, `client/src/session/mod.rs` |
| 4 | **Instant find**: in-thread find plus message-text search over a local index, spanning imported Claude/Codex/Cursor history | T3 search is server-side, two-character minimum; local index plus virtual list gives per-keystroke results across everything you ever ran | M | `ui/src/transcript.rs`, `shell/thread_search.rs`, `client/src/session/transcript.rs`, `engine/src/workspace_search.rs` |
| 5 | **Native PR review and merge** via `gh`, with agent handoff on each thread | the PR pane already has tree, activity, checks and handoff; adding review submit, resolve and merge makes review a 120 fps native flow | L | `engine/src/source_control.rs`, `ui/src/pull_request_detail.rs`, `pull_request_interactions.rs`, `rpc/src/lib.rs` |
| 6 | **PR babysitter agent**: GitHub trigger for always-on agents (poll or webhook) that fixes failing checks | T3's `watch_pull_request` lives inside one thread and stops at settle; an agent with memory and inbox survives restarts | M | `engine/src/agents.rs`, `proto/src/agents.rs`, `engine/src/source_control.rs` |
| 7 | **Phone approvals with push**: APNs through the self-hosted edge plus the existing Live Activity target | approvals already reach the phone; push makes "agent waiting" actionable from the lock screen | M | `edge/src`, `apps/ios/ZeronLiveActivity`, `crates/mobile/src/client_ffi` |
| 8 | **Parallel accounts via isolated CLI homes** (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`) chosen per chat, plus instance settings (binary, env, args) | removes the one-account-live limit (T3 has it); Godspeed already shows every account's usage meters, so per-chat pick plus limit-aware ordering is a step past T3 | M | `engine/src/agent_accounts.rs`, `engine/src/registry.rs`, `settings/harnesses.rs`, `ui/src/account_usage.rs` |
| 9 | **Cloud sessions as child rows** (Cursor cloud, Codex cloud, Claude on the web) | T3 #15644 has 20 reactions and no answer; the sidebar already renders subagent rows and pills | L | `harness/src/cursor/shim.mjs`, `client/src/session`, `ui/src/running_pill.rs` |
| 10 | **HTTP MCP with OAuth**, and tools for worktrees, PRs, files and visuals | T3 sends hosted agents (ChatGPT, bots) through its Connect tunnel; Godspeed's IPC listener already serves HTTP and the self-hosted edge can supply the public URL | M | `crates/mcp`, `engine/src/rpc.rs`, `engine/src/auth.rs` |
| 11 | **Live history sync**: watch `~/.claude/projects`, Codex and Cursor stores so terminal-run sessions appear and resume automatically | T3 import is a one-time onboarding step; Godspeed's importer is already idempotent and keyed by native session id | S | `engine/src/chat_import/`, `engine/src/fs_watch.rs` |
| 12 | **Agent-driven preview browser** exposed as MCP tools with Appshot-style capture | native webview tabs and dev-server discovery exist; tools plus recording close T3's agent-checks-its-own-UI loop without Electron | L | `ui/src/browser/`, `crates/preview`, `crates/mcp/src/tools.rs` |
| 13 | **Thread inbox**: settle/snooze/auto-settle on merged PR (PR state is already tracked), undo toast, working duration and diff stats on rows | T3 lifecycle is built; Godspeed can use PR badge state and per-row data already in `client` to match it with less UI | M | `shell.rs`, `shell/sidebar_sections.rs`, `client/src/session/mod.rs`, `ui/src/change_requests.rs` |
| 14 | **First-class Linux and Windows**: Wayland-correct window, `.desktop`, winget and Homebrew cask, tray | T3 has 29 open Linux issues and 7 Wayland ones as an Electron app; GPUI is native on both | M | `dist/`, `scripts/package-linux.sh`, `apps/godspeed/src/main.rs` |
| 15 | **Worktree janitor**: age/merged/settled cleanup with dry-run report, using the existing orphan sweep and dirty guard | T3's policy is server-side and opaque; a native report with "why kept" per worktree is a clear UX win | M | `engine/src/repos.rs`, `settings/worktrees.rs` |
