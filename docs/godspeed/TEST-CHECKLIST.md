# Godspeed manual test checklist

For the release in `/Applications/Godspeed.app` (0.5.0). Tick each box; note anything off
with the section and item so it can be fixed in one pass. "Expect" says what correct looks like.

## 1. Launch and basics

- [x] Open from Spotlight. Expect: speedster icon in the Dock, window opens in under a second.
- [x] Quit and reopen. Expect: same window size/position, same chat selected.
- [x] Settings opens (Cmd+,). Expect: no blank pages.
- [ ] `godspeed status` in a terminal while the app is open. Expect: engine running, local only.

## 2. Imported chats

- [ ] Sidebar shows your projects (≈69) and chats (≈369) from T3, Claude, Codex and Cursor.
- [ ] Open a big imported T3 thread. Expect: full transcript, scrolls smoothly, images shown.
- [x] Send a message in an imported **Claude** chat. Expect: it continues the original session (it
      remembers earlier context).
- [x] Same for an imported **Codex** chat.
- [ ] Open an imported **Cursor** chat. Expect: readable history; a new message starts a fresh session.
- [ ] Settings → Import. Expect: sources listed with "already imported" counts; re-running imports
      nothing new.

## 3. Providers and accounts

- [x] Settings → Providers lists Claude, Codex, Cursor, Grok, OpenCode, Pi, Antigravity, Devin,
      Hermes, Muse with installed/not-installed state.
- [x] New chat on **Claude**: model picker shows your models; effort/fast options work.
- [x] New chat on **Codex**: models load; reasoning effort works.
- [x] **Cursor**: Settings → Accounts → connect (browser sign-in). Then a chat runs.
- [ ] **Muse**: after `muse login`, a chat runs. Without login: clear "run muse login" message.
- [ ] Account usage ring in the composer shows plan usage; clicking switches accounts if you have two.

## 4. Composer

- [ ] Type and send with Enter; Shift+Enter makes a newline.
- [ ] `/` opens commands (incl. `/compact`); `$` opens skills (your project skills appear even with
      your large personal skills folder); `@` mentions files.
- [ ] Paste an image. Expect: image chip, sent with the message.
- [ ] Paste a very long log (>20k chars). Expect: becomes a `pasted-text.txt` chip.
- [ ] Paste code containing `@decorator`. Expect: stays plain text, no mention chip.
- [ ] Drop a folder and a >24 MB file. Expect: their paths inserted as code.
- [ ] Voice dictation (Cmd+D) transcribes into the composer.

## 5. Access modes and approvals

- [ ] Access chip next to the model picker; Cmd+Shift+M cycles Supervised → Auto-accept edits →
      Auto → Full access. Default for new chats: Auto-accept edits.
- [ ] Supervised + ask the agent to run a shell command. Expect: approval card replaces the composer
      with the exact command, folder, and buttons; Enter approves, Esc declines.
- [ ] Ask for a file edit in Supervised. Expect: card shows a coloured diff.
- [ ] "Approve for this session" then a similar command. Expect: not asked again.
- [ ] Sidebar row shows amber "Needs approval"; a notification/sound fires.
- [ ] Cursor/Pi: unsupported modes are greyed out, not silently upgraded.
- [ ] Narrow the window below ~600 px. Expect: chip hides; the shortcut still works.

## 6. Running agents

- [ ] Long reply streams smoothly; tool calls collapse into summaries ("Ran 3 commands…").
- [ ] "Show full output" on a command reveals its stdout.
- [ ] Send while the agent works. Expect: message queues; you can reorder, edit, remove, or steer it.
- [ ] Stop (Esc if enabled) ends the turn; a typed answer to a question survives Stop.
- [ ] Agent asks a question. Expect: question card, 1–9 keys pick options.
- [ ] `/compact` in Claude. Expect: "Context compacted" note, context meter drops.
- [ ] Background subagents/shells running after the reply. Expect: row says "Waiting on N tasks",
      and the done sound waits until they finish.
- [ ] Quit (Cmd+Q) with an agent running. Expect: "N agents are running" confirm.

## 7. Sidebar and thread lifecycle

- [ ] Row menu: Settle → chat moves to the Settled shelf; Unsettle brings it back.
- [ ] Snooze → 1 hour (and a typed time like "in 2m"). Expect: Snoozed shelf shows wake time; it
      returns on its own when the time passes.
- [ ] Wake now; Mark as unread / read.
- [ ] Cmd+Z right after a settle/snooze undoes it (not while typing in a field).
- [ ] Working row shows "Working · Nm"; with the Changes pane open, rows show +/- counts.
- [ ] Settings → General → auto-settle (off by default): turn on, pick 1 day, check it skips pinned,
      unread and running chats.
- [ ] Pin, rename, archive, delete still work.

## 8. Search

- [ ] Cmd+K, type one letter of something said in an old chat. Expect: results from message text
      with highlighted snippets, from the first character.
- [ ] Choose a result. Expect: opens that chat scrolled to that message.
- [ ] Sidebar magnifier opens the palette on Threads.
- [ ] In a chat, Cmd+F: "N of M", Enter/Shift+Enter step, Aa toggles case, Esc closes.

## 9. Git

- [ ] Changes pane shows branch with ahead/behind.
- [ ] Commit with an empty message → generated message matching the repo's style (conventional
      prefixes if the repo uses them); no "Co-Authored-By" lines.
- [ ] Commit & push; Pull (fast-forward only); Sync.
- [ ] Create PR dialog: generated title/body you can edit, base branch, draft toggle; success toast
      with a link; the PR appears in the PR board.
- [ ] In a folder with no repo: Init. In a repo with no remote: Publish (private).
- [ ] Error cases read clearly (e.g. push rejected, not signed in to gh).
- [ ] Branch label updates live after switching branch in a terminal.

## 10. Worktrees

- [ ] New chat in a new worktree; setup script output appears in a "(setup)" terminal.
- [ ] Settings → General → Worktrees: branch prefix field saves; invalid prefix shows an error.
- [ ] Unused worktrees list: delete one; a dirty one shows the error and "Delete anyway".
- [ ] Delete a chat that owns a worktree: "Also delete its worktree" checkbox works; dirty case
      offers "Delete anyway".

- [ ] Add an `env` section via Actions → Environment… (copy `.env*`, ports count 2, a setup step).
      Expect: validation messages for bad input; "Import from godspeed.json" works.
- [ ] New worktree chat with that env. Expect: `.env` copied, session card shows a Ports row
      (`:41000–41001`), `echo $GODSPEED_PORT` in its terminal prints the first port.
- [ ] A failing setup step. Expect: worktree kept, "Setup failed at step N" in the session card.
- [ ] Sleep the worktree from the session card. Expect: its dev server stops, "Sleeping" row and a
      "sleeping" tag in the branch picker; opening the chat wakes it with a Wake terminal tab.

## 11. Panels and tools

- [ ] Right panel tabs: files, diff, browser, PR, terminal. Cmd+Shift+T reopens the last closed tab.
- [ ] Diff: working tree / branch / latest turn scopes; split/unified; fold all; review comment on a
      line becomes a composer chip; scroll/folds kept when switching chats and back.
- [ ] File browser + editor with autosave; markdown rendered/source toggle.
- [ ] Terminal (Cmd+J): tabs, scrollback, copy/paste.
- [ ] Preview browser opens a local dev server.
- [ ] Shortcuts: Cmd+[ / Cmd+] back/forward between chats, Cmd+Shift+G changes pane, Cmd+Shift+D
      settle, Cmd+B sidebar, Cmd+1–9 jump to chat. Settings → Shortcuts lists them and lets you rebind.

## 12. Zoomies (Settings → Zoomies)

- [ ] Create a zoomie with an interval trigger (e.g. every 15 min) and Run now. Expect: a normal chat
      appears; the inbox shows a summary.
- [ ] Cron trigger with your timezone shows the right next run.
- [ ] Webhook trigger: copy the URL, `curl -X POST` JSON to it. Expect: a run whose prompt includes
      `{{body.field}}` values; a repeat with the same `Idempotency-Key` doesn't run twice.
- [ ] File-watch trigger fires on a file change in its folder.
- [ ] Supervised zoomie asking approval: inbox shows needs-approval and waits (no auto-deny).
- [ ] Zoomie memory file (`~/.godspeed/agents/<id>/MEMORY.md`) is updated by the zoomie.

## 12b. Needs-you inbox

- [ ] Inbox entry at the top of the sidebar with a count when a chat needs approval or has finished
      unseen.
- [ ] j/k moves, a approves, d declines, Return opens the chat. Expect: approvals show the exact
      command/diff with a risk colour (e.g. `git push` is high).
- [ ] Shift+A approves only the low-risk items; high-risk ones stay.
- [ ] Question timeout (Settings → General) answers a question after N minutes; an approval never
      times out.
- [ ] Telegram: four agent approvals within a minute arrive as one digest message.

## 13. Usage-limit recovery

(Only testable when an account actually hits its limit.)
- [ ] Limited chat shows the violet "Limited" status and a banner: Resume at reset / Snooze until
      reset / Dismiss.
- [ ] Resume at reset continues the same chat after the reset; messages sent meanwhile go after it.
- [ ] Switching account before the reset makes it ask instead of auto-resuming.

## 14. Delegation

- [ ] Ask an agent to spin off a side chat with notify on (via the Godspeed MCP tools). Expect: when
      the child finishes, fails or needs approval, the parent gets a notice and continues; a notice
      never approves anything.

## 15. Appearance

- [x] Light/dark/system; pick a built-in theme; import a VS Code theme file.
- [x] Chat width and font size settings apply.
- [ ] Nothing still says "Zeron" in the UI (report any you see).

## 15b. Speedster Dock icon

- [ ] Nothing running: the Dock icon is the speedster, breathing slowly (one frame every ~2 s).
- [ ] One chat working: jogs. Four or more: sprints with lightning.
- [ ] An approval or question: waves. A chat finishes: fist pump for ~2 s. A failed turn: trips for ~2 s.
- [ ] Chats already finished at launch don't celebrate.
- [ ] Reduce Motion or Low Power Mode: icon holds still.
- [ ] Settings → Appearance → Animated Dock icon off: normal app icon returns at once.
- [ ] Looks good at your Dock size (the art has no icon padding).

## 15c. Mission Control, receipts, usage (0.5.0)

- [ ] Cmd+Shift+K opens Mission Control over a frosted backdrop; Esc closes. Header clears the
      traffic lights; the speedster and four totals show.
- [ ] Cards: working ones have a moving edge, needs-you ones pulse, a chat finishing while open
      fades green. hjkl/arrows move, Return opens, `/` filters, `a` approves only low-risk.
- [ ] A finished turn ends with "Receipt · …"; clicking opens the slip (light and dark paper, torn
      edge, lines print in). Copy and Copy as image paste correctly. Settings → General toggle hides it.
- [ ] Composer tachometer: needle readable, red zone from 90%, kicks when a turn starts; hover shows
      windows; click opens Settings → Usage.
- [ ] Settings → Usage lists every account with bars, reset countdowns and Switch.
- [ ] A Limited chat: "Continue on another account…" opens a new "(continued)" chat with the prompt
      typed in; the original is untouched.

## 15d. Add a project to another device

Needs two devices signed into the same workspace (this Mac plus another machine or a headless
server running `godspeed headless`).

- [ ] New session canvas: pick a project that only this Mac has, open the device chip. Expect: the
      Mac's checkout, then "Add to <other device>…" rows. An offline device's row is dimmed with
      the offline glyph and a tooltip saying it is offline.
- [ ] Right-click the project in the projects dropdown. Expect: the same "Add to …" rows.
- [ ] Pick one. Expect: dialog with the route (Mac path → other device), a folder defaulting to the
      same place under home (`~/…`), the clone URL with an SSH/HTTPS toggle, and the project's
      git-ignored files with `.env`, `.env.*`, `.envrc`, `.dev.vars` already ticked and sizes shown.
      `node_modules` and build folders are not listed; files over 1 MB can't be ticked.
- [ ] Toggle HTTPS on an SSH remote. Expect: the URL rewrites (`git@github.com:o/r.git` →
      `https://github.com/o/r.git`). A remote with a custom port keeps the toggle disabled.
- [ ] Clone. Expect: progress in the dialog (reading files, receiving objects %, adding), then the
      app lands on the new project on the other device with no toast; the composer shows
      "Copying N files…" and holds sends until it clears. The ticked files exist in the new
      checkout; nothing tracked was overwritten.
- [ ] The new project groups with the original in the project picker (one row, two devices).
- [ ] Clone into a non-empty folder. Expect: an error in the dialog, Retry after fixing the folder.
- [ ] Stop during a slow clone. Expect: the dialog returns to editing; the folder on the other
      device is gone (or left empty if it existed).
- [ ] Subdirectory project (a monorepo package): adding it clones the repo and opens the package.

## 16. Robustness

- [ ] Quit mid-run and reopen. Expect: chat recovers, no duplicate messages.
- [ ] Sleep the Mac during a run, wake it. Expect: app recovers, snoozes/schedules fire late but once.
- [ ] Unplug the Lexar drive: the app itself doesn't depend on it (only the dev build cache does).
