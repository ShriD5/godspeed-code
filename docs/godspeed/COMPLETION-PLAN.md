# Completion pass

Approved scope: finish the handoff's remaining local desktop workflows, preserve the existing UI,
and measure feature coverage and performance against T3 Code. This is not a public release.

## Constraints

- Leave the active resume-at-reset worktree and its cache untouched until its owner finishes.
- Use isolated worktrees, at most two builds total, four Cargo jobs per build, external APFS caches.
- No GUI launches, synthetic input, screenshots, or tests using the owner's agent/app data.
- Preserve typography, spacing, panels, animations, and existing interaction patterns.
- Prefer focused modules and explicit names; avoid new explanatory comments unless necessary.
- Test lifecycle, persistence, authorization, and failure behavior; avoid redundant implementation tests.
- Commit reviewed slices with conventional messages and push only to private origin.
- Keep decisions cut in HANDOFF.md out of scope. No account fallback, provider switching, relay,
  checkpoints, telemetry, or automatic public release.

## Ordered work

1. Reliable delegation: opt-in notify on MCP child creation; parent wakes on completion/failure/input;
   durable delivery, deduplication, explicit-stop protection, visible pending work.
2. Thread lifecycle: settle/snooze/wake, mark unread, scoped undo, optional inactivity cleanup,
   working duration and available diff counts. Reuse existing sidebar and attention surfaces.
3. Reproducible T3 comparison: pinned feature audit and isolated headless performance runs, raw
   measurements, environment/build identity, distributions and limitations. No GUI-performance claim
   from server measurements or unmatched live process snapshots.
4. Native PR actions: review submission and thread resolution, explicit merge/auto-merge with fresh
   head checks and failure feedback. Tests use fakes, never real PR writes.
5. Finish existing worktree/settings affordances and high-value keyboard actions. Avoid adding blank
   pages or controls without working backend behavior.
6. Review integrated changes, run required headless checks, package the release, update handoff and
   comparison docs, and push validated work. Owner's visual review and signing remain separate.

## Evidence required

Every completed slice records its commit, tests, limitations, and review. Feature coverage references
source paths and never promotes a row from a design alone. Benchmark results distinguish engine
latency, CLI startup, memory, provider task time, and GUI responsiveness. Only the measured categories
get numbers. Runs under concurrent builds are discarded as performance evidence.
