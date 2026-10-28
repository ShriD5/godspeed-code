# T3 Code features Godspeed doesn't have yet

The active gap list. Audited 2026-10-28 against T3 Code `8e76f945` and Godspeed `main`, by reading
both codebases (two audit passes: desktop app, and server / agents / git hosts / mobile). Out-of-scope
items (see `T3-WATCH.md`: plan mode, rewind/checkpoints, account fallback, mid-thread provider
switching, T3 Connect relay, SnapShots, cookie import) are excluded.

**How this list stays current:** the T3 watch zoomie updates it on every run: when it ports a gap it
moves the row to "Done" with the PR number, and it adds rows for new T3 features. The Theo watch
zoomie's notes (`~/.local/share/godspeed-ops/theo-watch.md`) feed it. Rows marked MISSING were
searched for under several names; PARTIAL means Godspeed has a smaller version.

## Top 10 (ranked by value for people running several agents)

1. ~~**Agents drive the in-app browser**~~ Done in #57 (`preview_*` MCP tools; recording and appearance still missing).
2. ~~**PR review and merge from the app**: approve / request changes, reply and resolve threads, merge, auto-merge, mark ready.~~ Done (see Done).
3. ~~**PR watch that wakes the chat** on failed checks, new review comments and merge conflicts.~~ Done (see Done).
4. ~~**Sidebar multi-select with bulk actions**~~ Done in #55.
5. ~~**Sub-agents in their own worktree** from MCP `create_chat`~~ Done (#59).
6. **Remote machines over SSH / Tailscale**: install and run the engine on a server (ROADMAP "Next" 1).
7. ~~**Worktree cleanup policies and a storage page**~~ Done (#59).
8. ~~**More agents via the ACP registry** (Factory Droid, Copilot CLI, Kimi, Gemini CLI) and **several instances of one agent** (accounts, binaries, custom models).~~ Done in #62 (uvx agents and Droid/Copilot-specific extensions still missing).
9. ~~**Composer prompt recall (↑) and prompt stash (⌘S).**~~ Done in #55.
10. ~~**Dock badge for chats awaiting input**~~ (done in #55), plus iPhone Live Activities per running agent.


## Desktop app

| Area | Feature (what the user can do) | T3 evidence | Godspeed status | Value |
|---|---|---|---|---|
| Desktop | Process list with resource use; stop or force-kill a process | `apps/web/src/components/settings/ProcessSignalActions.tsx`; `DiagnosticsSettings.tsx` | MISSING: no process view or signal actions in `crates/ui` | med |
| Desktop | Diagnostics page: logs, connection diagnosis, copy trace ID | `apps/web/src/components/settings/DiagnosticsSettings.tsx` ("Copy trace ID") | MISSING: only the connection health dot (`shell.rs`); no logs view | med |
| Desktop | Open a prompt link: new thread pre-filled with prompt, repo and model | `apps/web/src/components/CommandPalette.tsx` ("Copy prompt link", "...with repo and model"); `apps/desktop/src/app/DesktopLinuxUrlHandler.ts` | MISSING: `godspeed://open/chat/...` opens existing chats only (`crates/ui/src/links.rs`) | med |
| Desktop | Zoom the whole UI with Cmd+ / Cmd- / Cmd0 and from the View menu | `apps/desktop/src/window/DesktopApplicationMenu.ts` ("Zoom In", "Zoom Out", "Actual Size") | PARTIAL: font sizes in Settings → Appearance (`settings.rs` `ui_font_size`); no keyboard or menu zoom | med |
| Desktop | Native menus: File menu (Settings, Close Window), Edit "Paste as Text", View full screen and zoom | `apps/desktop/src/window/DesktopApplicationMenu.ts` | PARTIAL: App, Edit, View (appearance only) and Window menus in `crates/ui/src/app_menus.rs`; no File menu, no full-screen, zoom or paste-as-text item | low |
| Desktop | Pause background polling when the screen locks, on battery, or in low-power mode; configurable intervals | `apps/web/src/components/settings/BackgroundActivitySettings.tsx`; `apps/desktop/src/electron/ElectronPowerMonitor.ts`; `packages/contracts/src/settings.ts` (`BackgroundActivityOverrides`) | MISSING: Low Power Mode only stops animations (`crates/ui/src/speedster.rs`) | low |
| Desktop | Quit by holding the shortcut; quit-confirmation modes | `apps/desktop/src/window/QuitHold.ts`; `apps/web/src/components/QuitHoldOverlay.tsx`; `settingsSearch.ts` ("Quit shortcut") | PARTIAL: confirms only while agents run (`shell.rs`, "agents running" dialog); no hold or double-press mode | low |
| Desktop | Update track: stable or nightly | `apps/desktop/src/updates/updateChannels.ts`; `settingsSearch.ts` ("Update track") | MISSING: one release feed (`crates/ui/src/app_update.rs`) | low |
| Desktop | Release notes shown with an available update | `apps/desktop/src/updates/releaseNotes.ts`; `apps/web/src/components/sidebar/SidebarUpdateReleaseNotes.tsx` | MISSING: update strip says "ready · Restart" with no notes (`app_update.rs`) | low |
| Desktop | Update all providers and the app from one dialog | `apps/web/src/components/sidebar/UpdateEverythingDialog.tsx` | PARTIAL: agent-update strip and per-harness policies (`shell/harness_updates.rs`); no combined action | low |
| Desktop | Install the command-line tool from inside the app | `apps/desktop/src/app/DesktopCliShim.ts`; `settingsSearch.ts` ("t3 command") | MISSING: CLI is installed by hand; `godspeed daemon install` only installs the engine service (`apps/godspeed/src/daemon.rs`) | low |
| Desktop | Open-source licenses page in the app | `apps/web/src/components/settings/OpenSourceLicenses.tsx` | PARTIAL: `THIRD_PARTY_NOTICES.md` in the repo only | low |
| Settings | Search across all settings pages | `apps/web/src/components/settings/settingsSearch.ts` (one index of every setting and page) | MISSING: 13 visible pages, no search (`crates/ui/src/shell.rs` `SettingsSection`) | med |
| Settings | Storage page: artifact and log usage, cleanup report, "Delete now" | `apps/web/src/components/settings/StorageSettings.tsx` ("Artifacts and logs", "Delete now") | MISSING: no storage usage or cleanup UI | med |
| Settings | Per-project overrides: default model, env mode, merge method, scripts, branch naming, auto-settle | `packages/contracts/src/settings.ts` (`ProjectSettingsOverrides`); `apps/web/src/components/settings/ProjectSettingsPanel.tsx` | MISSING: settings are global or per device (`crates/ui/src/settings.rs`); no per-project overrides | med |
| Settings | Labs and beta page | `apps/web/src/routes/_chat.settings.labs.tsx` | MISSING | low |
| Settings | Keybindings: about 60 commands, `when` clauses, keybindings file | `packages/shared/src/keybindings.ts`; `apps/web/src/keybindings.ts` | PARTIAL: 24 rebindable actions plus 9 jump slots with a recorder and conflict refusal (`crates/ui/src/settings.rs` `ShortcutId`); no file, no `when` | low |
| Keyboard | Missing shortcuts: pin, copy thread reference, copy PR number, open in favorite editor, previous worktree, host/workspace/branch pickers, send in background, stash, steer or edit queued message | `packages/shared/src/keybindings.ts` (`thread.pin`, `thread.copyReference`, `pullRequest.copyNumber`, `editor.openFavorite`, `composer.previousWorktree`, `composer.host`, `composer.workspace`, `composer.branch`, `composer.sendBackground`, `composer.stash`, `thread.steerQueuedMessage`, `thread.editQueuedMessage`) | MISSING as shortcuts; the underlying actions are mostly menu or button only | low |
| Keyboard | Cycle focus between sidebar, pane and composer with F6 | `packages/shared/src/keybindings.ts` (`workspace.focusNext`) | PARTIAL: focus regions exist (`crates/ui/src/shell/navigation_focus.rs`); no cycle key | low |
| Palette | Restart agent session, copy prompt link, link a PR to a thread, open project settings, triage an issue | `apps/web/src/components/CommandPalette.tsx` | MISSING: palette has threads, files, commands, theme and project actions (`crates/ui/src/shell/command_palette.rs`) | low |
| Appearance | Contrast slider for the theme | `apps/web/src/appearanceContrast.ts`; `settingsSearch.ts` ("Contrast") | MISSING | med |
| Appearance | In-app theme editor (colors, export JSON) | `apps/web/src/components/settings/ThemeEditorHost.tsx`; `packages/shared/src/keybindings.ts` (`themeEditor.toggle`) | PARTIAL: edit theme JSON on disk, then Reveal (`crates/ui/src/theme_library.rs`) | low |
| Appearance | Search Open VSX for themes and import them | `apps/web/src/openVsxThemes.ts`; `apps/web/src/components/settings/ThemeSearchSection.tsx` | PARTIAL: import a local VS Code theme (`theme_library.rs`); no marketplace search | low |
| Appearance | Diff color scheme | `settingsSearch.ts` ("Diff colors") | MISSING: diff colors come from the theme | low |
| Appearance | Separate prompt font | `settingsSearch.ts` ("Prompt font", "Separate fonts") | PARTIAL: UI, code and terminal fonts (`settings/appearance.rs`) | low |
| Appearance | Time format (relative or absolute, 12 or 24 hour); font smoothing; environment-identification badge | `apps/web/src/timestampFormat.ts`; `settingsSearch.ts` ("Time format", "Font smoothing", "Environment identification") | MISSING | low |
| Sidebar | Regenerate a thread's title on demand | `apps/web/src/components/Sidebar.tsx` ("Failed to regenerate thread title") | MISSING: manual rename only (no regenerate RPC in `crates/engine/src/titles.rs` or `crates/proto`) | med |
| Sidebar | Auto-settle a thread when its PR merges; keep threads that have an open PR | `settingsSearch.ts` ("Auto-settle merged threads", "Keep threads with a PR"); `packages/contracts/src/settings.ts` (`sidebarAutoSettleOnMerge`) | PARTIAL: auto-settle after 1 day, 3 days or 1 week idle only (`crates/ui/src/settings/thread_lifecycle.rs`); no merged-PR trigger, no keep-PR rule | med |
| Sidebar | Compact idle threads before the prompt cache expires | `packages/contracts/src/settings.ts` (`idleCompaction`); `settingsSearch.ts` ("Compact idle threads") | MISSING (T3-WATCH lists it as queued) | med |
| Sidebar | Unsent-draft rows in the sidebar, with open and discard | `apps/web/src/components/sidebar/SidebarDraftBlock.tsx` | MISSING: drafts persist per chat but never appear as sidebar rows | low |
| Sidebar | Copy a thread's branch name or relative path (Copy submenu) | `apps/web/src/components/Sidebar.tsx` ("Branch copied", "Path copied") | PARTIAL: Copy submenu has Path, Godspeed link, harness link, harness session ID (`shell.rs` `render_chat_copy_submenu`); no branch, no relative path | low |
| Composer | Send in background, or send and open a new thread | `packages/shared/src/keybindings.ts` (`composer.sendBackground`, `composer.sendAndNewThread`) | MISSING | low |
| Composer | Default setting: a follow-up queues or steers the running turn | `packages/contracts/src/settings.ts` (`followUpBehavior`); `settingsSearch.ts` ("Follow-up behavior") | MISSING: chosen per message, no default setting | low |
| Composer | Send shortcut modes (Enter / Mod+Enter multiline / Mod+Enter) | `packages/contracts/src/settings.ts` (`sendShortcut`) | PARTIAL: Enter or Mod+Enter (`settings.rs` `ComposerSendBehavior`) | low |
| Composer | Collapse the composer to one line while scrolling the conversation | `apps/web/src/components/chat/ChatComposer.tsx`; `packages/contracts/src/settings.ts` (`composerCollapseOnScroll`) | MISSING | low |
| Composer | Hide or reorder models in the picker | `packages/contracts/src/settings.ts` (`hiddenModels`, `modelOrder`); `apps/web/src/modelPickerVisibility.ts` | MISSING: favorites only (`crates/ui/src/settings/composer.rs`) | med |
| Composer | Model picker keys: Mod+1-9 inside the picker; Mod+Shift+arrows to switch provider | `packages/shared/src/keybindings.ts` (`modelPicker.*`) | PARTIAL: arrow-key navigation in the compact picker (`crates/ui/src/pickers/compact.rs`) | low |
| Composer | Attach a pull request to the prompt as a context chip | `apps/web/src/components/composerContextPresentation.tsx`; `apps/web/src/components/chat/UserMessageContextChips.tsx` | MISSING | low |
| Composer | Code block language picker in the composer | `apps/web/src/components/chat/ComposerCodeBlockLanguagePicker.tsx` | MISSING | low |
| Composer | Show skills in the slash menu (toggle) | `settingsSearch.ts` ("Show skills in slash menu") | MISSING as a setting | low |
| Chat | Run a code block's command in the terminal | `apps/web/src/components/chat/MarkdownCodeBlock.tsx` ("Run in terminal") | MISSING | med |
| Chat | Fork a new thread from any response | `apps/web/src/components/chat/TimelineAssistantRows.tsx` ("Fork from this response") | PARTIAL: side chat forks the conversation through its latest completed reply (`crates/ui/src/shell/side_chats.rs` `fork_chat`); no pick of an earlier response | med |
| Chat | Response streaming mode: turn or paragraph | `packages/contracts/src/settings.ts` (`ResponseStreamingMode`) | MISSING: live streaming only | low |
| Chat | GitHub alert blocks and `<details>` collapsibles in replies | `apps/web/src/markdownGithubAlerts.test.tsx`; `apps/web/src/components/chat/MarkdownDetails.tsx` | MISSING: `crates/ui/src/markdown/` has no alert or details rendering | low |
| Chat | Inline video: play video attachments and video in replies | `apps/web/src/components/media/MediaVideoPlayer.tsx` | MISSING | low |
| Chat | Related threads: link threads to each other, see them in the details panel | `apps/web/src/components/chat/ThreadRelationshipsControl.tsx` | MISSING: session card only (`crates/ui/src/shell/session_info.rs`) | low |
| Terminal | Split a terminal side by side or stacked; split horizontally or vertically | `apps/web/src/components/ThreadTerminalDrawer.tsx` ("Split Terminal Horizontally", "Single / Stacked / Side by side"); `packages/shared/src/keybindings.ts` (`terminal.split`) | MISSING: one terminal per tab (`crates/ui/src/terminal/panel.rs`) | med |
| Terminal | Right-click menu: copy, paste, select all, jump to latest, add selection to chat | `apps/web/src/components/ThreadTerminalDrawer.tsx` (context menu items) | MISSING: right-click goes to the PTY or nowhere (`terminal/panel.rs` `on_other_mouse_down`) | med |
| Terminal | Clickable links and file paths in terminal output | `apps/web/src/components/ThreadTerminalDrawer.tsx` ("Unable to open link", "Unable to open path") | MISSING: no link detection in `crates/ui/src/terminal/` | med |
| Terminal | Add terminal selection or output to the composer | `apps/web/src/components/chat/ComposerPendingTerminalContexts.tsx`; `ThreadTerminalDrawer.tsx` ("Add to chat") | MISSING | med |
| Browser | Device viewports: phone and tablet presets, rotate, drag to resize | `apps/web/src/components/browser/BrowserDeviceToolbar.tsx`; `BrowserViewportResizeHandles.tsx` | MISSING: browser toolbar is back, forward, reload, address, external, annotate (`crates/ui/src/browser/view.rs`) | med |
| Browser | Record the browser with key and mouse overlays; set frame rate | `apps/desktop/src/preview/RecordingCursor.ts`; `RecordingInput.ts`; `settingsSearch.ts` ("Browser recording frame rate") | MISSING | med |
| Browser | Zoom the page: in, out, reset (Mod+=, Mod+-, Mod+0) and default zoom | `packages/shared/src/keybindings.ts` (`preview.zoomIn`); `apps/web/src/components/preview/ZoomIndicator.tsx` | MISSING | low |
| Browser | Force the page light or dark | `settingsSearch.ts` ("Default browser appearance") | MISSING | low |
| Browser | Floating mini preview while you work elsewhere | `apps/web/src/components/preview/ThreadPreviewMiniPlayer.tsx`; `settingsSearch.ts` ("Auto-show floating preview") | MISSING | low |
| Browser | Browser URL history and a search engine for the address bar | `apps/web/src/browserHistoryStore.ts`; `settingsSearch.ts` ("Search engine") | MISSING | low |
| Browser | Choose the default browser profile; incognito | `settingsSearch.ts` ("Default browser profile") | PARTIAL: one per-user profile (`crates/ui/src/browser/profile.rs`); no picker, no incognito | low |
| Browser | Passkeys in the in-app browser | `apps/desktop/src/preview/PasskeyBridge.ts` | MISSING | low |
| Devices | Device panel: iOS Simulator and Android Emulator streams, with agent device tools | `apps/web/src/components/device/DevicePanel.tsx`; `settingsSearch.ts` ("Simulator support", "Agent device access") | MISSING | med |
| Devices | Pair a phone or another computer with a QR code | `apps/web/src/components/settings/AuthorizedClientsSettings.tsx`; `apps/web/src/components/ui/qr-code.tsx` | MISSING (backend-heavy; may belong to the engine half) | med |
| Devices | Add an SSH host, WSL distro or Tailscale endpoint as a device | `apps/desktop/src/ssh/DesktopSshEnvironment.ts`; `apps/desktop/src/wsl/DesktopWslBackend.ts`; `apps/desktop/src/backend/tailscaleEndpointProvider.ts` | MISSING (backend-heavy; may belong to the engine half) | low |
| Devices | Expose the engine on the network from a setting | `apps/desktop/src/backend/DesktopServerExposure.ts`; `settingsSearch.ts` ("Network access") | MISSING (backend-heavy; may belong to the engine half) | low |
| Git | Auto-pull on a timer; git fetch interval | `settingsSearch.ts` ("Automatically pull", "Git fetch interval"); `packages/contracts/src/settings.ts` (`automaticGitFetchInterval`) | MISSING: manual fetch-all only | med |
| Git | Writing style for commit, PR and branch text | `packages/contracts/src/settings.ts` (`sourceControlWritingStyle`); `settingsSearch.ts` ("Source control writing style") | PARTIAL: generated text is repo- and template-aware (`crates/engine/src/text_generation.rs`); no style setting, one shared model | low |
| Git | Start new worktrees from the remote default branch (origin) | `packages/contracts/src/settings.ts` (`newWorktreesStartFromOrigin`); `settingsSearch.ts` ("Start from remote") | MISSING | low |
| Git | Submodule handling for new worktrees (modes) | `settingsSearch.ts` ("Submodules") | MISSING | low |
| Git | Git hosts other than GitHub (GitLab, Bitbucket, Azure DevOps, Forgejo, GitCafe); token sign-in without `gh` | `packages/source-control-gitlab/`, `packages/source-control-bitbucket/`, `apps/web/src/components/settings/SourceControlAppSignInSettings.tsx` | MISSING: GitHub through `gh` only (`crates/engine/src/source_control.rs`) | low |
| PR | Board filters: open, closed, merged; sort by readiness, updated, size; labels; authors | `apps/web/src/components/pullRequest/pullRequestListOptions.ts`; `PullRequestListFilters.tsx` | PARTIAL: Authored, All, Reviewing; stars; attention triage; open PRs only (`crates/ui/src/pull_requests.rs`) | med |
| PR | Convert to draft; delete branch; update branch from base; approve workflow runs | `PullRequestActionsMenu.tsx` ("Convert to draft", "Delete branch", "Update branch with the base branch", "Approve workflows to run") | MISSING (ready for review, close, reopen and auto-merge are done) | med |
| PR | Review guide tab written by an agent (model and instructions settings) | `apps/web/src/components/pullRequest/PullRequestGuideTab.tsx`; `settingsSearch.ts` ("Review guides (beta)") | PARTIAL: a review handoff sends the PR to a new chat (`crates/ui/src/pull_request_handoff.rs`); no guide tab, model or instructions | low |
| PR | Manually link a PR to a thread, or unlink it | `apps/web/src/components/pullRequest/LinkPullRequestDialog.tsx` ("Link pull request to thread", "Unlink from thread") | PARTIAL: PRs attach by branch or transcript link (`crates/ui/src/shell/chat_pull_requests.rs`); no manual link | low |
| PR | Reviewer, label, title and body editing | `PullRequestReviewerPicker.tsx`; `PullRequestLabelPicker.tsx`; `PullRequestTitleEditor.tsx` | MISSING | low |
| PR | Mark files as viewed in the PR Code tab | `apps/web/src/components/pullRequest/filesViewedStore.ts` | MISSING | low |
| PR | Stacked PRs: view a stack, merge stack, rebase stack | `apps/web/src/components/pullRequest/PullRequestStackMenu.tsx`; `PullRequestStackLayers.tsx` | MISSING | low |
| PR | Checks popover with rerun | `apps/web/src/components/pullRequest/PullRequestChecksPopover.tsx` | PARTIAL: checks tab (`crates/ui/src/pull_request_detail.rs`); no rerun | low |
| Diff | Hide whitespace-only changes | `packages/contracts/src/settings.ts` (`diffIgnoreWhitespace`, default on); `settingsSearch.ts` ("Hide whitespace changes") | MISSING | med |
| Diff | Changes pane as a folder tree | `apps/web/src/components/chat/ChangedFilesTree.tsx`; `apps/web/src/components/DiffPanel.tsx` | MISSING: no folder tree in `crates/ui/src/changes.rs` (the PR Code tab does have one) | med |
| Diff | Default state for new diff files (collapsed) | `packages/contracts/src/settings.ts` (`diffFilesCollapsed`) | PARTIAL: per-file fold (`changes.rs`); no default setting | low |
| Onboarding | Welcome wizard: connect agents, check installs, import projects | `apps/web/src/components/onboarding/WelcomeWizard.tsx`; `apps/web/src/routes/welcome.tsx` | MISSING: an import prompt on first launch only (PARITY-T3 notes) | low |

## Excluded (not gaps)

- Decided out in `docs/godspeed/T3-WATCH.md`: plan mode, rewind and edit-from-here, checkpoints, account fallback, mid-thread provider switching, T3 Connect, guest share, mobile compatibility, SnapShots, browser cookie import, usage and cost page, Telegram.
- Skipped in the T3-WATCH ledger: PDF, SVG, audio and office previews, T3's settings-as-modal design, Mermaid (Godspeed already renders it).
- No T3 counterpart, so not gaps: menu-bar tray item, launch at login, keep the Mac awake. T3 has no tray, login item or keep-awake code.
- Already present (no row): element picker and annotation chips (`crates/ui/src/browser/annotate.js`), cite selection (`transcript.rs` `render_cite_button`), mark unread (`shell.rs` `CHAT_MENU_ROW_UNREAD`), settle with an undo toast (`shell/thread_lifecycle.rs` `remember_lifecycle_undo`), custom snooze time (`SnoozeCustom`), working duration and diff stats on sidebar rows (`shell/spaces.rs`), composer list continuation and renumbering (`composer_markdown.rs`, `composer.rs` `newline_edit`), copy message, find in thread, sidebar pin drag.

## Stale rows in docs/godspeed/PARITY-T3.md

These rows say MISSING or PARTIAL but the code has them now: mark unread, settle undo toast, custom snooze, working duration and diff stats on rows, element picker and annotations, cite selection into composer, auto-settle of inactive threads, composer list continuation. T3-WATCH lists list continuation and renumbering as queued; both look implemented (`composer_markdown.rs:300`, `composer.rs:3785`). Worth a check before a PR is opened for it.

## Server, agents, git hosts and mobile

| Area | Feature (what the user can do) | T3 evidence | Godspeed status | Value |
|---|---|---|---|---|
| Source control | Mark ready for review, back to draft, close, reopen; delete the branch after merge | `pullRequest.ts` (actions `ready`, `draft`, `close`, `reopen`, `delete-branch`) | MISSING: `gh pr create --draft` only (`source_control.rs`) | Med |
| Source control | Update a PR branch from its base (merge or rebase) | `pullRequest.ts` (action `update-branch`, `updateMethods`) | MISSING | Med |
| Source control | Request reviewers, see reviewer candidates, set labels | `PullRequestProvider.ts` (`setReviewerRequest`, `listReviewerCandidates`, `setLabels`) | MISSING (PARITY section 13 agrees) | Med |
| Source control | Mark files viewed, synced to the host | `pullRequest.ts` (`viewedFiles: "host"`); `PullRequestProvider.ts` (`getFilesViewed`, `setFilesViewed`) | MISSING | Med |
| Source control | Stacked PRs: see the stack and rebase or restack the whole stack | `pullRequest.ts` (`stacks`, `stackActions`); `packages/source-control-github/src/server/githubStackRebase.ts`; GitCafe CAPABILITIES `stacks: true` | MISSING | Med |
| Source control | PR review guide: an AI tour of a PR's changes, readable in the PR pane | `apps/server/src/pullRequest/PullRequestReviewGuideService.ts`; MCP `write_pull_request_guides`, `read_pull_request_guide` | MISSING | Med |
| Source control | Link a PR to a thread by hand; the thread settles when its PR merges or closes | `apps/server/src/orchestration-v2/ThreadPullRequestService.ts`; `ThreadSettlementService.ts`; MCP `link_pull_request` | PARTIAL: branch-detected PR badge only. Auto-settle is inactivity-only (`crates/engine/src/chat_lifecycle.rs`) | Med |
| Source control | Merge and close actions also reach the thread: agent-run `gh pr merge` is detected and the thread settles | `packages/source-control-core/src/shared/definition.ts` (`changeRequestCloseCommand`, `changeRequestCreateCommand`) | MISSING | Low |
| Source control | Approve pending Actions runs on fork PRs; revert a merged PR | `pullRequest.ts` (actions `approve-workflows`, `revert`) | MISSING | Low |
| Source control | PR change breakdown: +/- split into code, tests and docs | `apps/server/src/pullRequest/pullRequestChangeBreakdown.ts` | MISSING | Low |
| Source control | GitLab merge requests: list, detail, create, comment, review, merge | `packages/source-control-gitlab/src/server/GitLabPullRequestProvider.ts`, `GitLabCli.ts` | MISSING: GitHub only via `gh` | Med |
| Source control | Bitbucket pull requests | `packages/source-control-bitbucket/src/server/BitbucketPullRequestProvider.ts`, `BitbucketApi.ts` | MISSING | Low |
| Source control | Azure DevOps pull requests (via `az`) | `packages/source-control-azure-devops/src/server/AzureDevOpsPullRequestProvider.ts`, `AzureDevOpsCli.ts` | MISSING | Low |
| Source control | Forgejo / Gitea pull requests | `packages/source-control-forgejo/src/server/ForgejoPullRequestProvider.ts` | MISSING | Low |
| Source control | GitCafe pull requests with native stacks | `packages/source-control-gitcafe/src/server/GitCafePullRequestProvider.ts` | MISSING | Low |
| Source control | Sign in to a forge in Settings (GitHub App or token), without `gh` | `apps/server/src/sourceControl/SourceControlSignIn.ts`; `packages/source-control-github/src/server/GitHubAppSignIn.ts`; `GitHubCredentials.ts` | MISSING (PARITY section 13: "GitHub token without gh") | Med |
| Providers | Claude reset credits: see the balance and redeem one when limited | `apps/server/src/provider/claudeResetCredits.ts`; `resetCreditCoordinator.ts`; `packages/contracts/src/providerUsageLimits.ts` (`ServerProviderResetCredits`) | MISSING | Med |
| Providers | Idle compaction: Claude threads compact before the prompt cache expires (global default, per-instance override, beta) | `apps/server/src/orchestration-v2/IdleCompaction.ts`; `IdleCompactionService.ts`; `packages/contracts/src/orchestrationV2.ts` (`idleCompaction`) | MISSING (T3-WATCH queued it) | Med |
| Providers | Claude workflow progress: phases and per-agent state rows | `apps/server/src/orchestration-v2/Adapters/claudeWorkflowProgress.ts` | MISSING: no `workflow_progress` handling in `crates/harness/src/claude` | Low |
| Providers | Provider-native workflow scripts can be run and stopped; the script sits behind a disclosure | `packages/contracts/src/orchestrationV2.ts` (`workflowScript`); MCP `t3_workflow_stop` | MISSING (PARITY section 14) | Low |
| Providers | Sign in to ChatGPT without installing the Codex CLI | `apps/server/src/provider/CodexManagedRuntime.ts`; `CodexChatGptAuth.ts`; `CodexManagedTokenBroker.ts` | PARTIAL: wraps the CLI login only (PARITY section 1) | Low |
| Orchestration | Agent drives a device simulator or emulator: open, screenshot, close | `apps/server/src/mcp/toolkits/device/tools.ts`; `apps/server/src/device/DeviceService.ts` | MISSING | Med |
| Orchestration | Delegated tasks with a task id: check status, cancel, and the child wakes the parent on completion or question | `orchestrator/tools.ts` (`task_status`, `task_cancel`); `apps/server/src/orchestration-v2/DelegatedTaskQuestionReactor.ts`; `apps/server/src/orchestration-v2/ThreadManagementService.ts` | PARTIAL: `create_chat` + `wait_for_turn` only (PARITY section 4) | High |
| Orchestration | Delegate a child to another linked machine, and it reports back | `apps/server/src/peer/RemoteDelegation.ts`; orchestrator `t3_environment_links` | PARTIAL: `create_chat` takes a `device` (`crates/mcp/src/tools.rs`); no wake-back | Med |
| Orchestration | Agent schedules, lists, pauses, deletes and runs always-on tasks | `orchestrator/tools.ts` (`schedule_task`, `list_scheduled_tasks`, `delete_scheduled_task`); `thread/tools.ts` (`run_scheduled_task_now`) | MISSING: MCP has about 16 tools and none schedule (`crates/mcp/src/tools.rs`) | Med |
| Orchestration | Agent asks the user for a secret in a private card and gets a one-use reference; the model never sees the value | `orchestrator/tools.ts` (`request_secret`) | MISSING (PARITY section 3) | Med |
| Orchestration | Agent moves its own thread into a new worktree on a new branch | `apps/server/src/mcp/toolkits/worktree/tools.ts` (`t3_worktree_handoff`, `t3_worktree_status`, `t3_worktree_list`) | MISSING (PARITY section 12: "no handoff") | Med |
| Orchestration | Webhook tasks verify an HMAC signature and dedupe deliveries | `apps/server/src/scheduledTasks/webhookVerification.ts`; `webhookDedupe.ts` | MISSING: no verification (signature headers are only redacted in `crates/engine/src/agents/prompt.rs`) | Med |
| Orchestration | Remote machines over SSH: the app installs and runs the engine on a server through an SSH tunnel, with password prompt | `packages/ssh/src/tunnel.ts` (`SshEnvironmentManager`); `packages/ssh/src/auth.ts`; `apps/server/src/device/SshDeviceHost.ts` | MISSING: SSH is only the git clone protocol (`crates/ui/src/shell/add_to_device_ui.rs`). ROADMAP "Next" 1 plans "Run on a server" | High |
| Orchestration | Tailscale: detect the tailnet, serve the engine on it, pick an exit node | `packages/tailscale/src/tailscale.ts`; `apps/server/src/environment/DirectEndpoints.ts` | MISSING (PARITY section 17) | Med |
| Orchestration | Agent asks a thread's history questions: search message text across all threads | `apps/server/src/mcp/toolkits/thread/tools.ts` (`t3_thread_search`) | MISSING for agents. In-app search exists (`crates/engine/src/message_index.rs`) | Low |
| Orchestration | Agent pins, snoozes, settles, archives or marks unread its own thread | `thread/tools.ts` (`t3_thread_organize`) | PARTIAL: `archive_chat` only | Low |
| Orchestration | Agent sends files into a thread | `apps/server/src/mcp/toolkits/attachment/tools.ts` (`t3_thread_send_attachments`) | MISSING | Low |
| Orchestration | Thread title regenerated on demand | `apps/server/src/orchestration-v2/ThreadTitleRegenerationService.ts`; `regenerateTitle` in `packages/contracts/src/orchestrationV2.ts` | MISSING (PARITY section 5). Titles are generated once (`crates/engine/src/titles.rs`) | Low |
| Orchestration | Forked thread: merge its work back into the source thread | `thread/tools.ts` (`merge_back`); `apps/server/src/orchestration-v2/ThreadForkService.ts` | MISSING (PARITY section 4) | Low |
| Orchestration | Triage a GitHub issue with a playbook session | `apps/server/src/triage/triageSession.ts`; `triagePrompt.ts`; `apps/mobile/src/features/triage/useStartTriage.ts` | MISSING | Low |
| Mobile | Agent Live Activity (Dynamic Island and Lock Screen) per running thread: starting, running, waiting for approval or input, completed, failed | `apps/mobile/src/widgets/AgentActivity.tsx`; `apps/mobile/src/features/agent-awareness/agentLiveActivity.ios.ts` | MISSING: only voice and usage Live Activities (`apps/ios/GodspeedLiveActivity/`) | High |
| Mobile | Android app | `apps/mobile/modules/t3-agent-notifications/android/`; `apps/mobile/modules/t3-native-controls/android/` | MISSING (PARITY section 16) | Med |
| Mobile | Share sheet: send text, links or images from another app into a new thread | `apps/mobile/src/features/sharing/IncomingShareProvider.tsx`; `incoming-share-*.ts` | MISSING: no share extension in `apps/ios` | Med |
| Mobile | Prompt links: open the app on a new thread with prompt, repo, PR, provider and model filled in | `apps/mobile/src/features/prompt-links/prompt-link-model.ts` (keys `prompt`, `repo`, `pr`, `provider`, `model`); `newTaskPromptLink.ts` | MISSING: deep links open a chat only (`crates/ui/src/links.rs`; `apps/ios/Godspeed/App/SceneDelegate.swift`: voice, usage, inbox, board) | Med |
| Mobile | Terminal on the phone: native terminal, paste, send a selection to the chat | `apps/mobile/src/features/terminal/ThreadTerminalRouteScreen.tsx`; `NativeTerminalSurface.tsx`; `modules/t3-terminal` | MISSING. The engine already has remote terminals (`crates/engine/src/remote_terminals.rs`) but no phone UI | Med |
| Mobile | Git actions on the phone: overview, commit, branches, confirm | `apps/mobile/src/features/threads/git/GitOverviewSheet.tsx`, `GitCommitSheet.tsx`, `GitBranchesSheet.tsx`, `GitConfirmSheet.tsx` | MISSING | Med |
| Mobile | Review the diff on the phone, with line comments | `apps/mobile/src/features/review/ReviewSheet.tsx`; `ReviewCommentComposerSheet.tsx`; `modules/t3-review-diff` | MISSING | Med |
| Mobile | Pull request on the phone: summary, checks, inline comments, review guide | `apps/mobile/src/features/pull-requests/` (PullRequestSheetContent, PullRequestInlineCommentScreen, guide/) | PARTIAL: PR badge opens the URL (`apps/ios/Godspeed/Session/CoreSessionSource.swift`) | Med |
| Mobile | File browser and previews on the phone: markdown, CSV, PDF, zip, audio, video | `apps/mobile/src/features/files/` (ThreadFilesRouteScreen, FileTreeBrowser, ZipArchiveView, DelimitedFilePreview, AudioFilePreview) | MISSING: images only (`ImageViewer.swift`) | Med |
| Mobile | Dev-server browser preview streamed to the phone | `apps/mobile/src/features/browser/` (BrowserPreviewRouteScreen, PreviewStreamWebView, ThreadBrowserFloat) | MISSING | Med |
| Mobile | Scheduled tasks editor on the phone: create, edit, run now, copy webhook URL | `apps/mobile/src/features/settings/SettingsScheduledTasksRouteScreen.tsx`; `scheduled-task-editor.tsx` | MISSING: no always-on UI in `apps/ios` | Med |
| Mobile | Device preview streamed to the phone; shake to open it | `apps/mobile/src/features/devices/` (DevicePreviewRouteScreen, shakeDetector.ts) | MISSING | Low |
| Mobile | Provider accounts on the phone: list, usage, add or switch | `apps/mobile/src/features/settings/SettingsProviderAccountsRouteScreen.tsx` | PARTIAL: per-account usage shown (`apps/ios/Godspeed/Usage/UsageViewController.swift`); no add or switch | Low |
| Mobile | Quick actions and Siri shortcuts: new thread, inbox | `apps/mobile/src/features/shortcuts/appShortcuts.ts` | MISSING | Low |
| Mobile | Pair the phone to a machine with a link or QR code | `apps/mobile/src/features/connection/pairing.ts` | MISSING: phone signs in through the edge only (PARITY section 17) | Low |

## Already present in Godspeed (checked, not gaps)

These T3 features were checked and Godspeed has them, so they are not rows above. Several contradict the older PARITY-T3.md.

- Settle, snooze, wake, mark unread and undo: `crates/ui/src/shell/thread_lifecycle.rs`. Opt-in auto-settle after N inactive days: `crates/engine/src/chat_lifecycle.rs`. PARITY section 6 is stale here.
- Find in thread and message-text search: `crates/ui/src/shell/find_bar.rs`, `crates/engine/src/message_index.rs`.
- Project icon from repo artwork (`public/favicon.svg`, `apple-touch-icon.png`): `crates/ui/src/shell/project_icon.rs`.
- Scheduled tasks with webhook triggers, run now, pause, rotate URL, and a file-watch trigger: `crates/engine/src/agents.rs`.
- Pinned, archived and grouped threads on the phone; settle and snooze swipes; group by project: `apps/ios/Godspeed/Threads/SessionListController.swift`.
- Phone notifications per event kind (done, input, failed): `apps/ios/Godspeed/Shell/MoreViewController.swift`, `App/PushNotifications.swift`.
- Usage home-screen widget: `apps/ios/GodspeedLiveActivity/UsageWidget.swift`. Voice Live Activity and Codex realtime voice plus dictation: `apps/ios/Godspeed/Voice/`, `Composer/Dictation.swift`.
- New project by browsing folders on a device: `apps/ios/Godspeed/Threads/NewProjectViewController.swift`.
- Open-chat deep link `godspeed://open/chat/...`: `crates/ui/src/links.rs`.
- iPad split layout and hardware key commands: `apps/ios/Godspeed/Shell/SplitRootController.swift`, `Composer/ComposerBar.swift`.
- Harnesses: Claude, Codex, Cursor, Grok, OpenCode, Pi, Muse, Antigravity, Devin, Hermes (`crates/proto/src/agent.rs`).
- Per-account usage meters and limit recovery: `crates/engine/src/limit_recovery.rs`, `crates/engine/src/agent_accounts/usage.rs`.
- Repo-aware PR body, branch and commit text generation: `crates/engine/src/text_generation.rs`.
- GitHub PR media, check rollups, and PR handoff for failing checks or conflicts: `crates/ui/src/pull_request_media.rs`, `crates/ui/src/pull_request_handoff.rs`.
- Local message index search and cross-device chat creation from MCP (`create_chat` with `device`).

## Skipped by decision or out of scope (not counted)

- Guest share and T3 Connect relay notifications and live activity: `apps/server/src/guest/`, `apps/server/src/relay/AgentAwarenessRelay.ts`. T3 Connect is out of scope.
- Cloud sign-in and self-update: `apps/server/src/cloud/`.
- Context handoff and mid-thread provider switching: `apps/server/src/orchestration-v2/ContextHandoffService.ts`. Out of scope per T3-WATCH.
- Goals, checkpoints and rewind, plan mode: out of scope per T3-WATCH and PARITY section 18.
- Document text extraction for PDFs and office files: `apps/server/src/provider/DocumentText.ts`. T3-WATCH skipped it on purpose.
- Jujutsu (`jj`): `packages/contracts/src/vcs.ts` declares the kind, but `apps/server/src/vcs/VcsDriverRegistry.ts` registers only the git driver. Not a shipped T3 feature.
- Claude ACP adapter as an alternate driver: Godspeed's native Claude driver covers the need.
- Notification action buttons: T3 has none (`apps/mobile/src` has no notification categories), so there is nothing to port.

## Done

| Feature | Godspeed PR |
|---|---|
| Run ACP-registry agents: install, update and uninstall from Settings → Providers → More agents (checksum-pinned binaries and integrity-checked exact npm versions; uvx not yet) | #62 |
| Factory Droid, GitHub Copilot CLI, Gemini CLI and Kimi CLI as ACP agents through the registry (generic ACP driver; Droid elicitation forms and Copilot subagent routing not ported) | #62 |
| Several instances per provider: binary, config dir, env vars (secrets in the keychain), default and custom models, account; picked in the model picker | #62 |
| Custom model ids per provider instance (hidden models still missing) | #62 |
| Agents drive the in-app browser: open, navigate, snapshot, screenshot, click, type, scroll, eval, console, wait (`preview_*` MCP tools, macOS). Recording and set-appearance remain open. | #57 |
| PR review and merge from the PR pane: approve / request changes / comment, reply to and resolve / unresolve threads, merge (allowed methods, head-pinned, confirmed), enable / disable auto-merge, ready for review, close / reopen (`crates/engine/src/source_control_actions.rs`, `crates/ui/src/pull_request_actions.rs`) | #56 |
| PR watch that wakes the chat on failed checks (with the failing log tail), new review comments and merge conflicts; Needs-you items; per-PR opt-in, on by default for PRs a chat opened; dedupe, hourly wake cap, backoff, idle pause; optional auto-merge when green and approved (`crates/engine/src/pr_watch.rs`) | #56 |
| Sidebar multi-select with bulk settle, snooze, read/unread, pin, archive, delete; one undo per batch | #55 |
| Composer prompt recall (↑/↓ through sent prompts) and per-chat prompt stash (⌘S, Stashed chip) | #55 |
| Dock badge with the number of chats waiting on an approval or answer | #55 |
| Sub-agents in their own worktree: MCP `create_chat` / `create_chats` `worktree` (`new`, `parent`, existing path or branch) | #59 |
| Worktree cleanup policies (merged, untouched 7/14/30 days; changes always kept) and a worktree storage view with Delete now / Delete all eligible | #59 |
