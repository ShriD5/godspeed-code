# Upstream issue triage (Zeron + T3 Code)

2026-10-15. Every open issue on zeronsh/zeron (113) and pingdotgg/t3code (1,404) was read and
checked against Godspeed's code. About 750 apply (58 Zeron + ~700 T3, heavily duplicated into the
themes below); ~770 do not (mobile apps, T3 Connect relay and remote backends, Electron/web
specifics, T3's preview browser and device tools, Zeron's hosted cloud, features Godspeed cut).

Raw data and per-batch reports: `~/Developer/godspeed-issues/` (not committed).

## Batch 1: agent drivers and process hygiene

- Claude: show synthetic replies with no deltas (Z#734); compaction start/end as timeline events
  and a context reset (Z#801, T3 C10/J); close a subagent on an errored spawn result (Z#893); gate
  `--thinking-display` on CLI support (Z#477); surface `permission_denials` as needs-attention, not
  Completed (T3 Q); distinct "context full" error instead of "usage limit" (T3 19832); keep option
  descriptions/previews on questions (T3 14237); route MCP elicitation to the approval card (T3
  14878); clamp effort to `maxEffortLevel` (T3 E).
- OpenCode: kill the whole process tree, no orphaned `serve` (Z#764); settle background subagents
  (Z#845); question tool replies (Z#668); honour permission patterns in auto (T3 11458).
- Codex: respect the user's config (network access, reasoning summary) instead of overriding
  (T3 14952, 13987); cap a single JSON-RPC line (T3 12884).
- ACP: Antigravity model list vs `clientInfo.name` (T3 15378/16489).
- Hygiene: record agent process groups and reap leftovers at engine start (T3 15357/15542); clean
  Antigravity `_MEI*` temp dirs (T3 12239); run probes from a neutral cwd (T3 8818); strip OSC
  escapes from captured stdout (T3 7754); retry PATH capture after a timeout, nushell (T3 C5).

## Batch 2: engine, git, worktrees, terminal, automation

- "Done" sound/banner only when no background work is live, plus a "waiting on N tasks" state
  (T3 13625/15099, cluster C/A).
- Worktree creation lock per repo (T3 17198); never force-remove a dirty worktree; "delete
  worktree too?" when deleting a chat, orphan sweep (T3 C12/F).
- Git: SSH host aliases detected as GitHub (T3 17805); remotes not named `origin` (T3 15373);
  `GIT_TERMINAL_PROMPT=0` for background git (T3 13912); show `gh` stderr verbatim; live branch
  label via `.git/HEAD` watcher (Z#619); branch prefix setting (Z#132).
- Terminal: bounded per-subscriber channel (T3 18428); mouse/alt-screen wheel reporting (Z#361);
  answer OSC 10/11 colour queries (T3 15175).
- Always-on: error with partial text counts as Failed (agents.rs:1072); install id so a copied DB
  doesn't fire agents (T3 15513).
- Import: strip any leading `<tag>` block from Codex titles (T3 C17); per-file scan timeout, skip
  privacy-protected folders (T3 15971).
- Uploads: allow harness temp/scratch dirs in the image-preview jail (Z#885).
- Usage: never publish an empty window list after a failed probe (T3 12170).

## Batch 3: UI

- Markdown inverted-range panic (Z#892).
- Shimmer animates only for live runs (Z#485).
- Quit confirmation when agents are running in the embedded engine (T3 10161).
- Composer: large paste becomes a file chip (T3 C2); paste never creates mentions (T3 C9); typed
  answer survives Stop during a question (T3 8963); oversized files/folders sent by path (T3 17471).
- Per-chat scroll position restore (T3 L); diff scroll/fold state per chat (T3 C7).
- Visual `table` accepts `{key, header}` columns (Z#913); Codex file-citation directives as links
  (Z#424).

## Later (features, need a decision)

Resume-at-reset for usage limits (not account fallback); delegation that wakes the parent when a
child finishes or blocks (Z#768, T3 15082); GitHub PR trigger for always-on agents; custom/hidden
models; GitLab; ACP registry + Cline/omp/DeepSeek harnesses; cloud agent sessions as child rows
(T3 15644, 20 reactions); Windows polish; Linux `.desktop`; Homebrew cask; Intel build without
voice; i18n.

Already handled by Godspeed: no telemetry (T3 4123, 19 reactions), plugin skills (T3 5622, 16),
OpenCode context limit (T3 8388), remembered model, git actions (Z#473), chat import (Z#743),
approvals.
