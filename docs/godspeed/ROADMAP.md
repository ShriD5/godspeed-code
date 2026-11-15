# Godspeed roadmap

Godspeed is a fork of [Zeron](https://github.com/zeronsh/zeron) (MIT): one native app for every AI
coding agent, with T3 Code-level depth, zoomies (always-on agents) and a calm way to run many agents
at once. What shipped in each release is in the [changelog](../../CHANGELOG.md).

## Shipped

- **0.1:** rebrand and local-only fork; approvals and four access modes; git write actions with
  generated text; Muse Code driver; zoomies (cron, interval, webhook, file triggers,
  memory, inbox); chat import from T3 Code, Claude Code, Codex and Cursor; message search; resume
  at usage-limit reset; thread lifecycle (settle, snooze, undo); delegation that wakes the parent;
  about 50 Zeron and T3 Code issues fixed.
- **0.2:** Telegram bot for zoomies (open-dots style).
- **0.3:** opt-in cloud sync with WorkOS sign-in; signed and notarized macOS builds; iPhone app (in
  development).
- **0.4:** Needs-you inbox; worktree environments (ports, copied `.env` files, setup/teardown,
  sleep); the animated speedster Dock icon.
- **0.5:** Mission Control; turn receipts; usage cockpit, tachometer and usage history with cost at
  API prices; continue a limited chat on another account; new icon.

## Next

1. **Close the laptop, the work keeps going.** Linux builds and the "Run on a server" setup (SSH or
   Tailscale) are built and land in the next release; still to do: the end-to-end soak on a real
   VPS and reconnect hardening so the phone, desktop and Telegram hand a chat back and forth
   without hiccups.
2. **PR autopilot.** Review, reply, resolve and merge from the PR pane; per-PR autopilot that feeds
   failing checks and review comments back to the chat and merges when green.
3. **iPhone app on TestFlight**, with push notifications for approvals.
4. **Windows and Linux builds** of the desktop app.
5. **Multi-repo workspaces** for worktree environments.

## Not planned

- Plan mode, automatic account fallback and switching providers in the middle of a chat (they break
  prompt caching; continuing on another account is an explicit choice instead).
- Maybe later: checkpoints, rewind and edit-from-here.
