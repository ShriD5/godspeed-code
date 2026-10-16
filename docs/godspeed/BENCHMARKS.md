# Headless benchmarks: Godspeed vs T3 Code

Run on 2026-10-16 (IST). Six fresh-profile trials per app, interleaved
(G T T G, repeated three times). No trial was discarded or failed.

This compares the **headless servers only**: Godspeed's `godspeed headless`
engine and T3 Code's packaged `serve` server, both driven over their own
loopback WebSocket RPC by the same small Python client. It does not measure
GUI rendering, window memory, or agent/model task speed. See
[Limitations](#limitations).

## Environment

| | |
|---|---|
| Machine | MacBook Air, Mac14,2, Apple M2 (4P + 4E cores), 16 GB |
| OS | macOS 27.0 (26A428), Darwin 27.0.0 |
| Godspeed | 0.2.113 (the version inherited from Zeron; reset to 0.1.0 later), `/Applications/Godspeed.app/Contents/MacOS/godspeed`, release build made 20 minutes after `d752f8cd` (main) |
| Godspeed binary SHA-256 | `e8e44b698147c56ea10421b27e103cdaa4f8ced867daa67ed9899a598416388d` |
| T3 Code | 0.0.40 (installed `/Applications/T3 Code (Alpha).app`), Electron 43.4.1, Node 24.18.1 |
| T3 launcher SHA-256 | `65f5609a31dac03f7891bb27ba06c585f93cc930e545a9ff29bb0986af9eea66` |
| T3 `app.asar` SHA-256 | `c0b6f1174303e893930039b185573c1894f7ace48ba7f165b0256ab68c0c2725` |
| T3 Electron Framework SHA-256 | `1f2c239db00e2844d39f995e1101536a760ef165b0b40304322c13351e878af8` |

The installed T3 is 0.0.40. The T3 source checkout used for parity work
(`339179f5`) is 0.0.45. The RPC calls here were written against the 0.0.40
server source, recovered from the sourcemap that ships inside the installed
`app.asar`. 0.0.40 has no `projects.ensureScratch` or `projects.list`, for
example.

**The machine was not idle.** No `cargo` or `rustc` ran during any trial
(checked before and after each one). But an orphaned `vitest` worker from
another project used about 115% of one core the whole time. Chrome was active,
and the owner's T3 Code GUI stayed open (the benchmark never touched its
data). The 1-minute load average at trial start ranged from 3.5 to 12.8.
Interleaving spreads this noise across both apps, but absolute numbers on an
idle machine would be somewhat lower, especially the cold-start times.

## Method

Each trial does the following:

1. Creates a fresh `mktemp -d /tmp/bench-<app>-XXXX` holding `home/`, `data/`,
   `tmp/`, `project/` and `projects/`. The owner's `~/.godspeed`, `~/.t3`,
   `~/.claude` and `~/.codex` are never read or written. Picks a free
   loopback port.
2. Spawns the server with a scrubbed environment (`env -i` equivalent) and
   `PATH=/usr/bin:/bin:/usr/sbin:/sbin`, so no agent CLI is reachable.
3. Measures **cold start**:
   - Godspeed: from just before `Popen` until `EngineReady` returns on the
     IPC socket.
   - T3: from just before `Popen` until `server.getConfig` returns. That
     includes waiting for the server to print its pairing token, one
     `POST /oauth/token` exchange, and the WebSocket upgrade with a bearer
     token, which is what any T3 client must do.
   - "Port accepting" (first successful TCP connect) is reported separately
     for both.
4. Keeps one WebSocket client connected and idles:
   - At ready + 5 s and ready + 30 s, sums RSS (`ps`) and physical footprint
     (`footprint -f bytes --noCategories <pids…>`) over the server's whole
     process tree, and counts its processes.
   - Idle CPU is the tree's CPU-time delta (`ps time`) from ready + 10 s to
     ready + 30 s.
5. Runs the operations below. Each latency is one request/response round trip
   on the open socket, timed with `perf_counter` in the client. 200
   repetitions each, except `list_201p_400t` (50).
6. Re-samples memory 2 s after the last operation.
7. Shuts down (Godspeed `StopEngine`; T3: close the client, then `SIGTERM`).
   Checks that no process from the tree is left (none were), then deletes the
   temp dir (all were removed).

### Operations

| Name | Godspeed RPC | T3 RPC |
|---|---|---|
| `list_*` | `WatchSpaces` + `WatchChats`, both pipelined; time until both initial snapshots arrive, then cancel | `orchestration.subscribeShell`; time until the first `Chunk` (snapshot), then `Interrupt` |
| `create_thread` | `Mutate {op:createChat, spaceId}` | `orchestration.dispatchCommand {type:thread.create}` |
| `create_project_thread` | `Mutate createSpace` then `Mutate createChat` (2 round trips) | `dispatchCommand project.create` then `thread.create` (2 round trips) |
| `search_*` | `SearchMessages {query, limit:50}` | `orchestration.searchThreads {query, limit:50}` |

The store grows in this order: empty → 1 project with 200 threads
(`list_1p_200t`) → plus 200 project+thread pairs (`list_201p_400t`) → plus
the seeded corpus.

**Seeded search corpus.** Both apps import Claude Code transcripts through a
public RPC, so the same files seed both stores without starting a provider:

- Godspeed: `RunChatImport {source:"claude"}`.
- T3: `project.create` for the corpus folder, then
  `agentSessions.import {projectId}`.

`seed_corpus.py` writes 100 sessions × 20 messages (2,000 messages, 12–40
words each, deterministic seed) into the trial's temp `HOME/.claude/projects`.
Both apps imported all 100 sessions and 2,000 messages in every trial. Queries:

- `kestrel`: planted in 39 messages across 34 sessions.
- `build`: in 698 messages, all 100 sessions.

The two searches have different semantics, so hit counts differ:

- Godspeed returns per-message word-prefix hits from its transcript index:
  39 for `kestrel`, and 50 (the limit) for `build`.
- T3 runs a SQLite `LIKE '%q%'` scan and returns at most one match per
  thread. It only considers assistant messages linked to a turn, and imported
  history has no turns, so it effectively searched user messages: 21 for
  `kestrel`, 50 for `build`.

The latencies are each app's real cost for its own search call over the same
imported text. They do not measure identical work.

### Provider safety

No model provider was started.

- Godspeed ran with `GODSPEED_HARNESS=mock`. The only agent CLI at a
  hard-coded path on this Mac is Homebrew's `muse`, so `MUSE_EXECUTABLE`
  pointed at a missing file. Without that override the engine runs `muse` and
  `brew info --cask muse-code` at startup as a version/update probe.
- T3 ran with `T3CODE_TELEMETRY_ENABLED=false`. Its Claude health check
  failed with "not found", as intended.
- Both apps spawn a login shell at startup to capture PATH. With the temp
  HOME, that finds no agent CLIs.
- The child processes that appear after operations are `git`, `lsof` and
  `ps` from each app's repository and process probes.

### Commands

The launch shapes as run:

```sh
# Godspeed
env -i HOME=$T/home GODSPEED_DATA_DIR=$T/data GODSPEED_IPC_PORT=$PORT \
  GODSPEED_HARNESS=mock MUSE_EXECUTABLE=$T/no-provider/muse \
  PATH=/usr/bin:/bin:/usr/sbin:/sbin SHELL=/bin/zsh TMPDIR=$T/tmp/ LANG=en_US.UTF-8 \
  /Applications/Godspeed.app/Contents/MacOS/godspeed headless

# T3 Code (cwd = $T/project)
env -i HOME=$T/home T3CODE_HOME=$T/data ELECTRON_RUN_AS_NODE=1 \
  T3CODE_TELEMETRY_ENABLED=false PATH=/usr/bin:/bin:/usr/sbin:/sbin \
  SHELL=/bin/zsh TMPDIR=$T/tmp/ LANG=en_US.UTF-8 \
  "/Applications/T3 Code (Alpha).app/Contents/MacOS/T3 Code (Alpha)" \
  "/Applications/T3 Code (Alpha).app/Contents/Resources/app.asar/apps/server/dist/bin.mjs" \
  serve --base-dir $T/data --host 127.0.0.1 --port $PORT --no-browser $T/project
```

The full run is `python3 -I run_all.py`, then `python3 -I analyze.py`, from
the bench folder.

## Results

Each cell gives the median across the 6 trials, with the min–max of the
per-trial values in parentheses.

### Startup, memory, CPU

| Metric | Godspeed | T3 Code | T3 / Godspeed |
|---|---:|---:|---:|
| Spawn → port accepting (ms) | 128.8 (52.9–142.3) | 590.7 (552.6–652.8) | 4.6x |
| Spawn → first readiness RPC answered (ms) | 132.6 (53.5–146.0) | 876.5 (804.8–952.6) | 6.6x |
| RSS, ready + 5 s (MiB) | 31.6 (31.4–31.7) | 286.5 (286.1–287.0) | 9.1x |
| Physical footprint, ready + 5 s (MiB) | 11.2 (11.1–11.3) | 165.6 (165.0–166.0) | 14.8x |
| RSS, ready + 30 s (MiB) | 33.9 (33.7–34.0) | 286.9 (286.5–287.4) | 8.5x |
| Physical footprint, ready + 30 s (MiB) | 12.1 (11.8–12.1) | 166.0 (165.4–166.3) | 13.8x |
| Processes in tree, ready + 30 s | 1 | 1 | |
| Idle CPU, ready + 10 s → + 30 s (% of one core) | 0.10 (0.05–0.10) | 0.15 (0.10–0.15) | |
| RSS after all operations (MiB) | 116.4 (111.5–121.5) | 443.7 (423.8–450.9) | 3.8x |
| Physical footprint after all operations (MiB) | 36.2 (31.0–37.2) | 305.6 (301.4–307.9) | 8.4x |
| Processes in tree after all operations | 4 (3–4) | 5 (1–9) | |

Notes on this table:

- Godspeed's cold start is bimodal: two trials took about 53 ms and four took
  120–146 ms. The cause was not isolated. Background load or P-core vs E-core
  scheduling are plausible.
- For T3, 0.0.40 printed its token at a median of 826 ms. The token exchange
  plus WebSocket upgrade plus first call added about 45–55 ms more.
- Idle CPU is at the 10 ms resolution of `ps` CPU time over a 20 s window, so
  both are effectively idle.
- The post-operation figures include short-lived `git`/`lsof`/`ps` children,
  so they are less stable than the idle samples.

### Request latency (ms)

Each cell is the median of the 6 per-trial p50 (or p95) values, with the
min–max across trials in parentheses.

| Operation | Godspeed p50 | Godspeed p95 | T3 p50 | T3 p95 | T3/G p50 |
|---|---:|---:|---:|---:|---:|
| `list_empty` | 0.111 (0.107–0.115) | 0.150 (0.139–0.154) | 0.373 (0.355–0.394) | 0.683 (0.571–1.274) | 3.4x |
| `search_empty_store` | 0.050 (0.047–0.086) | 0.063 (0.059–0.154) | 0.290 (0.276–0.352) | 0.415 (0.384–1.255) | 5.8x |
| `create_thread` | 0.240 (0.234–0.255) | 0.433 (0.425–0.463) | 1.406 (1.353–1.485) | 6.288 (2.158–6.675) | 5.9x |
| `list_1p_200t` | 0.812 (0.763–0.837) | 0.859 (0.800–0.912) | 22.98 (22.77–24.50) | 25.36 (24.34–26.44) | 28x |
| `create_project_thread` | 1.953 (1.872–2.047) | 3.105 (2.929–3.350) | 6.307 (6.181–6.558) | 10.83 (9.97–12.68) | 3.2x |
| `list_201p_400t` (50 reps) | 1.824 (1.709–1.943) | 2.179 (1.922–10.85) | 1565 (879–1663) | 1739 (963–1819) | ~860x |
| `search_400_empty_threads` | 0.113 (0.095–0.131) | 0.863 (0.124–14.02) | 0.744 (0.693–0.874) | 2.191 (2.132–5.032) | 6.6x |
| `search_seeded_rare` (`kestrel`) | 0.451 (0.420–0.525) | 3.594 (0.593–5.695) | 3.986 (3.738–4.052) | 5.780 (5.606–6.305) | 8.8x |
| `search_seeded_common` (`build`) | 0.592 (0.529–0.597) | 0.703 (0.651–0.752) | 6.610 (6.511–7.884) | 8.449 (8.285–17.88) | 11x |

On the T3 shell snapshot at 201 projects: in 0.0.40, `subscribeShell`
resolves repository identity for every project on every snapshot (via `git`,
concurrency 4). For these non-git folders the cost did not go away on
repeated snapshots. A diagnostic
run (`probe_t3_list.py`) showed the cost is per project, not per thread, and
did not settle over 60 s of idle:

| Store | Snapshot time |
|---|---|
| 10 projects | ≈ 42 ms |
| 50 projects | ≈ 200 ms |
| 200 projects | ≈ 0.8–1.0 s |
| 1 project, 200 threads | ≈ 26 ms |

200 plain (non-git) folders as separate projects is an unusual shape for a
real user. The 1-project/200-thread row is closer to typical use.

### Other timings

These are not like-for-like:

- **Import of the 100-session corpus.** Godspeed: 836 ms (761–1116),
  including polling `SearchMessages` every 250 ms until the index reports
  settled. T3: 5,206 ms (4,716–6,558), a single synchronous
  `agentSessions.import` call. T3 caps an import at 100 transcripts and 200
  messages each, and only looks at sessions from the last 30 days. Godspeed
  imports everything.
- **Shutdown.** Godspeed `StopEngine` → exit: 899 ms (612–985), since it
  drains its runtime first. T3 `SIGTERM` → exit: 142 ms (132–148). Both exited
  cleanly in all 6 trials.

### Static footprint

| | Godspeed | T3 Code |
|---|---:|---:|
| App bundle on disk (`du -sk`) | 142.2 MiB | 437.3 MiB |
| App bundle, sum of file sizes | 149,096,628 B | 457,970,747 B |
| Main executable | 146,993,744 B (`godspeed`, arm64 Mach-O, UI + engine in one binary) | 53,328 B launcher, plus `Electron Framework` 191,416,176 B and `app.asar` 208,161,922 B |

## Raw data

Everything is in `/Volumes/Lexar/build/godspeed-coordination/bench/`, on the
owner's external drive, outside this repo:

- Results:
  - `results.jsonl`: one record per trial, with every sample, per-pid
    footprints, process names, latencies, import results, shutdown info and
    load average.
  - `results.csv`: one row per trial.
  - `summary.json`
  - `environment.txt`: OS, hardware, sizes, hashes, versions.
  - `run_all.log`
- Scripts:
  - `bench.py`: one trial.
  - `run_all.py`: interleaved schedule with the build check.
  - `analyze.py`
  - `rawws.py`: a stdlib WebSocket client. Godspeed's IPC rejects any
    handshake that carries `Origin`.
  - `seed_corpus.py`
  - `probe_t3_list.py` and `probe_t3_threads.py`: diagnostics.
- `pilot.jsonl`: one pilot trial per app, used to debug the scripts. It is
  not included in the results.

## Limitations

- **Headless server only.** There is no GUI. Godspeed's GPUI window and T3's
  Electron renderer/GPU processes are not running, so this says nothing about
  rendering smoothness, input latency, or the memory of either app as a user
  normally runs it. Godspeed's headless engine and its headed app are the
  same binary.
- **No agent work.** No provider, model call or turn was run. Nothing here
  measures task speed, streaming throughput, or provider process overhead.
  Provider discovery was blocked in both apps.
- **Versions.** T3 is the installed 0.0.40, not the 0.0.45 source used for
  parity review. Results may differ on newer T3 builds; the per-project
  snapshot cost in particular may have changed.
- **Godspeed build.** The binary is a release build installed 20 minutes
  after `d752f8cd`. Its version string (0.2.113) does not encode the commit.
- **Corpora.** The empty-store and empty-thread searches return no hits; they
  measure call overhead only. The seeded corpus is small (2,000 messages) and
  synthetic. The two search implementations differ in semantics (per-message
  word-prefix index vs one-match-per-thread `LIKE` scan over user messages for
  imported threads).
- **Client cost.** The client is synchronous Python, so its overhead (tens of
  microseconds) is part of every latency, equally for both apps. Godspeed's
  `list` uses two pipelined subscriptions while T3's uses one.
- **Machine noise.** Load average 3.5–12.8, as described above. The memory
  numbers are tight across trials. Cold start and some p95 tails are visibly
  affected.
- **Sample size.** Six trials per app. Ranges are reported instead of
  confidence intervals.
