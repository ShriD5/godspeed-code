# Headless benchmarks: Godspeed 0.5.9 vs Zeron 0.2.130

| Headline (median of 6 trials) | Godspeed 0.5.9 | Zeron 0.2.130 | Godspeed 0.2.0 (same run) | Doc reference: 0.2.0 / Zeron 0.2.113 ([BENCHMARKS-ZERON.md](BENCHMARKS-ZERON.md)) |
|---|---:|---:|---:|---:|
| Cold start to ready, back-to-back series (n = 10) | 133 ms | 144 ms | 132 ms | 65 / 65 ms |
| Idle memory (physical footprint, ready + 30 s) | 13.0 MiB | 11.2 MiB | 12.1 MiB | 12.2 / 10.6 MiB |
| Idle CPU, ready + 10 s → + 130 s | 121 ms (0.10% of a core) | 87 ms (0.07%) | 107 ms (0.09%) | not measured |
| Idle instructions retired, same 120 s | 114 M | 80 M | 64 M | not measured |
| Idle timer wakeups | 22.5 /s | 21.7 /s | 20.8 /s | not measured |
| Memory after the workload (400 chats, 201 projects) | 37.9 MiB | 47.5 MiB | 36.2 MiB | 33.9 / 33.2 MiB |
| CPU spent on the workload (engine + its `git` children) | 3.4 s | 6.2 s | 6.2 s | not measured |
| List 201 projects / 400 chats (p50) | 1.33 ms | 1.12 ms | 1.31 ms | 1.95 / 1.74 ms |
| Streamed reply, 5,000 deltas 1 ms apart: delta latency p50 / p95 | 62 / 117 ms | 62 / 117 ms | 62 / 117 ms | not measured |
| Engine CPU for 9 follow-up turns of 5,000 deltas | 658 ms | 528 ms | 611 ms | not measured |
| Follow-up turn, send → final event seen by the client | 41–46 ms | 57–66 ms | 54–60 ms | not measured |
| Engine footprint after 10 turns (4.3 MB transcript) | 78.9 MiB | 100.6 MiB | 100.1 MiB | not measured |
| Engine footprint after a restart and 11 reads of that transcript | 111.1 MiB | 75.5 MiB | 109.0 MiB | not measured |
| Read the 4.3 MB transcript, cold after restart / warm | 17.1 / 7.8 ms | 17.8 / 8.4 ms | 17.5 / 7.9 ms | not measured |
| App bundle on disk | 158.8 MiB | 205.4 MiB | 143.8 MiB | 143.8 / 137.0 MiB |

All three are official macOS arm64 release builds, run headless on an M2
MacBook Air with a fresh profile for every trial. Run on 2026-10-30,
14:04–15:21 IST, with a replacement trial and the follow-up probes at
16:21–16:35. 18 trials (6 per build, interleaved). One Godspeed 0.5.9 trial
overlapped another agent's `cargo` build and was replaced. No trial failed.

**Where Godspeed is heavier:**

1. **Memory after reloading a long transcript.** After a restart and 11 reads
   of a 4.3 MB transcript, the Godspeed engine holds 111 MiB against Zeron's
   75 MiB (1.47x). Godspeed 0.2.0 holds the same 109 MiB, so this is not new
   since the fork: Zeron got lighter here and Godspeed did not follow.
2. **Idle work.** The idle engine retires 42% more instructions than Zeron's
   (114 M vs 80 M in 120 s, with no overlap between trials). That is 77% more
   than 0.2.0. It also wakes 0.8 more times per second than Zeron. In
   absolute terms this is still tiny: about 0.1% of one core. Part of the
   gap is two new network jobs: the update check to github.com at + 20 s,
   and the telemetry ping at + 60 s, whose TLS connection to the Cloudflare
   edge then stays open. The rest is a higher steady baseline of 0.80 vs
   0.63 ms of CPU per second.
3. **Engine CPU per streamed turn.** It is 25% higher than Zeron's on burst
   and follow-up turns (55 vs 44 ms on the first burst turn, and 658 vs
   528 ms over nine follow-ups). Against 0.2.0 it is 8–9% higher. The first
   turn on a new chat also takes about 40 ms longer from send to the agent
   process streaming (136 vs 96 ms).

Smaller gaps: idle footprint is 1.8 MiB above Zeron's (+16%), up from a
1.6 MiB gap at 0.2.0. Listing 200–400 chats is 14–19% slower than Zeron and
unchanged since 0.2.0.

**Where Godspeed wins:**

- **The 400-chat workload** costs 45% less CPU (3.4 s vs 6.2 s for both
  Zeron and 0.2.0).
- **After the workload**, Godspeed ends 20% smaller (37.9 vs 47.5 MiB) and
  uses less CPU while idle (6.5% vs 10.2% of a core over the next 30 s).
- **Follow-up turns** reach the client 28% sooner: 41–46 ms against 57–66 ms
  from send to the final event. They are also dispatched to the agent in
  17.5 ms against 30.7 ms.
- **After streaming 50,000 deltas**, the engine footprint is 22% lower
  (78.9 vs 100.6 MiB).
- **The download is smaller**: 60.2 MB against 80.0 MB, and the bundle is
  158.8 MiB against 205.4 MiB. Zeron 0.2.130 ships a second 55 MB
  `Zeron Engine` binary.

**Ties:** cold start, per-delta streaming latency, transcript reads (cold and
warm), creates, shutdown, and startup on a populated profile.

This compares the **headless engines only** (`godspeed headless` vs
`zeron headless`), driven over loopback WebSocket IPC by the same Python
client. **The GUI was not measured** (see [GUI](#gui-not-measured)).

## Environment

| | |
|---|---|
| Machine | MacBook Air, Mac14,2, Apple M2 (4P + 4E cores), 16 GB |
| OS | macOS 27.0 (26A428), Darwin 27.0.0 |
| Godspeed | **0.5.9**, official asset `godspeed-0.5.9-macos-arm64-app.tar.gz` from `ShriD5/godspeed-code` tag `v0.5.9` (`729a6077`). `--version` prints `godspeed 0.5.9`. Developer ID signed (team `QPUV967B5U`) and notarized; `spctl` accepts it. |
| Godspeed tarball SHA-256 | `7798a9b20b995d228162911311e8ccf3bee471b0bb1dffd5f2e9b73fdddf86f6`, which matches both the release's `SHA256SUMS` and its `manifest.json` |
| Godspeed binary SHA-256 | `b818b94cfef87c1c0264a8d4d9bfe0d4f98bff73066d1ed0f1741cf414a9d5ee` (164,289,328 B) |
| Zeron | **0.2.130**, the latest release (published 2026-10-29). Official asset `zeron-0.2.130-macos-arm64-app.tar.gz` from `zeronsh/zeron` tag `v0.2.130` (`369d4281`). `--version` prints `zeron 0.2.130`. Developer ID signed (team `5XY3M483YQ`) and notarized. |
| Zeron tarball SHA-256 | `b000663adeb205349124400dace8dc0aaccad524b3881153514344af246406b6` (matches the release's `manifest.json`) |
| Zeron binary SHA-256 | `zeron` `ef0bda732d52a2543ed2d385cd511d73bfb1c4db6b1f19d4dbdad6691b1b69c3` (157,781,440 B). `Zeron Engine` `f46c59ab31cc23c95578817670b0ef0406e1cfb7d29224fcfa59ccdd0028a74f` (55,000,256 B; GUI background engine, not used by `zeron headless`) |
| Godspeed 0.2.0 (reference arm) | The same download as in `BENCHMARKS-ZERON.md`. Binary SHA-256 `e3c2b7ea87cd1bc39bb7c57bd7de7bb77b9935e347c2bee57ee9807f739b99d7`, re-verified. |
| Distance from the fork | Both descend from `v0.2.113` (`6ac88599`). Godspeed `v0.5.9` is 340 commits past it, and Zeron `v0.2.130` is 103 commits past it, with +164k/−16k lines in `crates/` and `apps/`. Godspeed has not merged upstream since the fork. |

**Binary location.** Each tarball was downloaded and extracted into its own
directory: `/Volumes/Lexar/tmp/perf-bench/{godspeed-dl,zeron-dl}/`. That is
an APFS USB SSD. Unlike the 0.2.0 run, binaries and temp profiles stayed on
the external drive and were not copied to the internal SSD (house rule: no
scratch on the internal disk). An untimed `--version` and one untimed
headless start/stop of each binary ran first, to warm the page cache.

**Machine state.** The machine was not idle, and nothing could be done about
it. The owner's Godspeed GUI, Chrome, Spotlight indexing (`mdworker`) and
other agents' sessions were running. Load at trial start was 1.4–8.6
(median 6.0 for Zeron and 0.2.0, and 3.8 for 0.5.9 after the replacement).
Each benchmark step ran through `~/.local/share/godspeed-ops/heavy.sh` (one
heavy job at a time). Before each trial, the harness waited up to 10 min
while `cargo`, `rustc`, `xcodebuild` or `swift-frontend` was running.

- Godspeed 0.5.9 trial 6 started at load 15.3, with a `cargo` build that had
  not gone through `heavy.sh`. It is excluded from the tables, and trial 7
  (load 2.6, no build) replaces it. Both records are in the raw data.
- 0.2.0 trial 6 saw a build start just before it ended. It was kept: its
  numbers are inside the other trials' ranges, except its tenth follow-up
  turn.

## Method

The harness is in `/Volumes/Lexar/tmp/perf-bench/scripts/`. `bench.py` runs
one trial, `run_all.py` runs the schedule, and `analyze.py` summarizes. It
reuses the client and workload of the 0.2.0 run (`rawws.py` is unchanged).
Process accounting changed: it now uses `proc_pid_rusage(RUSAGE_INFO_V4)`
through ctypes, which gives:

- exact CPU time in ns
- interrupt (timer) wakeups and package-idle wakeups
- instructions retired
- physical footprint and its lifetime peak

Memory is still sampled with `footprint`, as in the earlier docs.

Each trial has two phases, each on its own fresh profile:
`mktemp -d …/run/<phase>-<variant>-XXXX` with `home/`, `data/`, `tmp/` and
`project/`. The owner's `~/.godspeed`, `~/.zeron`, `~/.claude` and
`~/.codex` were never read or written.

### Phase 1: start, idle, workload (the 0.2.0 method)

`<binary> headless` runs with a scrubbed environment:

```sh
env -i HOME=$T/home GODSPEED_DATA_DIR=$T/data GODSPEED_IPC_PORT=$PORT GODSPEED_HARNESS=mock \
  MUSE_EXECUTABLE=$T/no-provider/muse PATH=/usr/bin:/bin:/usr/sbin:/sbin SHELL=/bin/zsh \
  TMPDIR=$T/tmp/ LANG=en_US.UTF-8 godspeed headless      # Zeron: same with the ZERON_ prefix
```

No agent CLI is reachable. Both apps run **as shipped**:

- No `*_EDGE_URL` override.
- Godspeed's telemetry is on (the default).
- Godspeed 0.5.9's update feed is GitHub Releases.
- Zeron's update feed is `edge.zeron.sh`.

Zeron 0.2.130 now requires an IPC bearer token, which it writes to
`$DATA_DIR/ipc-token` on every start. The client reads that file and sends
`Authorization: Bearer …`. Godspeed 0.5.9 has no IPC token.

1. **Cold start**: time from just before `Popen` until `EngineReady` is
   answered.
2. **Idle, ready → + 130 s**, with one WebSocket client connected:
   - `rusage` of the engine every second
   - `footprint` at + 5, + 30 and + 130 s
   - `lsof` (non-loopback sockets) at + 1, 20, 30, 60, 70, 90, 100 and 125 s
   - Idle CPU, wakeups and instructions are deltas over **+ 10 → + 130 s
     (120 s)**, summed over the process tree. The old + 10 → + 30 s window
     is also reported.
3. **Workload** (identical to the 0.2.0 run):
   - `list_empty` (×200)
   - 200 × `createChat` in one project, then `list_1p_200t` (×200)
   - 200 × (`createSpace` + `createChat`), then `list_201p_400t` (×50)
   - Every trial ended with exactly (1, 200) and then (201, 400) in the
     snapshots.
   - Memory is re-sampled 2 s after the last call.
   - The tree's CPU over the workload is recorded, along with 30 s of idle
     after it.
4. `StopEngine`. Then the harness checks that nothing from the tree is left
   (nothing was) and deletes the profile (all were removed).

### Phase 2: streaming with a fake agent (no model is called)

Both engines honor `CLAUDE_CODE_EXECUTABLE`
(`crates/harness/src/claude/mod.rs` in both trees). Phase 2 points it at
`fake_claude.py`, a stand-in for the Claude Code CLI that speaks
`--input-format/--output-format stream-json`. It:

- answers the `initialize` control requests
- confirms every stdin message the way CLI 2.1.3xx does:
  `command_lifecycle` queued/started, a replay of the user line, then
  `completed`
- streams N `text_delta` stream events, then the full assistant message and
  `result`

Each delta is about 87 bytes and starts with `[seq:emit_ns]`, so the client
can time each delta from emit to visibility. The last delta carries an end
marker.

The same fake runs unchanged on both apps, so Zeron is measured on the same
footing.

Without the lifecycle/replay frames, a follow-up turn on a live process
waited out the driver's 5 s `UNREPORTED_SETTLE` in both apps. The pilot hit
this, and it was fixed before the run.

The project is a git repo with one commit. The client sends a
`QueueCommand {kind: run, request: {harness: "claude-code", …}}` and watches
`WatchDocMessages` on the same socket. "Done" is the first frame in which the
assistant entry is `status: complete` and contains the end marker.

| Step | What |
|---|---|
| Paced | New chat, 5,000 deltas 1 ms apart (about 6.5 s of streaming): send → done, last delta emitted → done, per-delta emit → visible latency (p50/p95), engine CPU and wakeups (send → done + 2 s), frames and bytes on the wire, footprint growth |
| Burst | Second chat, 5,000 deltas with no pacing: the same metrics |
| Long transcript | Nine more 5,000-delta burst turns on the burst chat (50,000 deltas, a 4.3 MB transcript, about 14 MB data dir): per-turn send → done and engine CPU |
| Reads | 10 × `WatchDocMessages` initial `reset` of that chat (warm) |
| Restart | `StopEngine`, relaunch on the same profile: ready time, one cold read, 10 more reads, footprint 2 s later |

Engine CPU in phase 2 is the engine process only. The fake agent's CPU is
excluded.

### Schedule

`run_all.py` runs each round as `G Z O O Z G` (G = Godspeed 0.5.9,
Z = Zeron 0.2.130, O = Godspeed 0.2.0), three times. Each trial takes about
3.5 min.

Afterwards there were three follow-ups:

- the replacement trial
- a back-to-back cold-start series: 10 rounds × 3 builds, rotating order,
  start → ready → stop, with page-ins recorded
- four untimed idle attribution probes (`attrib.py`) with `RUST_LOG=debug`
  or `info`, one of them with `GODSPEED_TELEMETRY=0`

## Results

Each cell gives the median of 6 trials, with the per-trial min–max in
parentheses. G/Z is Godspeed 0.5.9 ÷ Zeron 0.2.130, so values above 1.00x
mean Godspeed is bigger or slower.

### Startup, idle memory, idle CPU

| Metric | Godspeed 0.5.9 | Zeron 0.2.130 | G / Z | Godspeed 0.2.0 | 0.5.9 / 0.2.0 |
|---|---:|---:|---:|---:|---:|
| Spawn → `EngineReady`, in-trial (ms) | 183 (62–1692) | 265 (232–1892) | | 171 (63–1444) | |
| Spawn → `EngineReady`, back-to-back series, n = 10 (ms) | 133 (66–313) | 144 (89–1619) | 0.93x | 132 (94–1433) | 1.01x |
| Physical footprint, ready + 5 s (MiB) | 11.84 (11.42–12.22) | 10.19 (10.00–10.31) | 1.16x | 11.38 (11.22–11.69) | 1.04x |
| Physical footprint, ready + 30 s (MiB) | 12.99 (12.75–13.27) | 11.19 (10.75–11.31) | 1.16x | 12.13 (11.61–12.49) | 1.07x |
| Physical footprint, ready + 130 s (MiB) | 13.32 (12.84–13.74) | 11.35 (10.86–11.49) | 1.17x | 12.63 (12.24–12.86) | 1.05x |
| RSS, ready + 30 s (MiB) | 37.8 (37.4–37.8) | 34.2 (29.4–34.3) | 1.10x | 34.6 (34.1–34.9) | 1.09x |
| Threads, ready + 30 s | 12 (12–13) | 12 (11–12) | | 12 | |
| Idle CPU, + 10 → + 130 s (ms) | 121 (81–142) | 87 (65–126) | 1.38x | 107 (62–117) | 1.12x |
| … as % of one core | 0.10 | 0.07 | | 0.09 | |
| Idle CPU, + 10 → + 30 s, the old window (% of a core) | 0.14 (0.07–0.17) | 0.13 (0.07–0.18) | | 0.10 (0.06–0.13) | |
| Idle instructions retired, 120 s (millions) | **114 (110–119)** | **80 (76–87)** | **1.42x** | 64 (62–67) | **1.77x** |
| Idle interrupt wakeups (per s) | 22.5 (22.1–22.6) | 21.7 (21.2–22.1) | 1.04x | 20.8 (20.4–21.1) | 1.08x |
| Idle package-idle wakeups (per 120 s) | 23 (2–69) | 25 (11–179) | | 21 (16–68) | |
| Steady-state idle CPU, median of per-second deltas, + 11 → + 130 s (ms/s) | 0.80 | 0.63 | 1.27x | 0.73 | 1.10x |
| `StopEngine` → exit, empty-ish profile (ms) | 562 (551–667) | 561 (553–593) | 1.00x | 565 (538–594) | |

### Workload: 400 chats in 201 projects

| Metric | Godspeed 0.5.9 | Zeron 0.2.130 | G / Z | Godspeed 0.2.0 | 0.5.9 / 0.2.0 |
|---|---:|---:|---:|---:|---:|
| CPU over the workload, engine + children (ms) | **3,426 (3,334–3,678)** | 6,241 (6,077–8,467) | **0.55x** | 6,245 (6,134–6,360) | 0.55x |
| Physical footprint after the workload (MiB) | 37.9 (32.9–43.0) | 47.5 (38.8–52.0) | 0.80x | 36.2 (33.9–41.4) | 1.05x |
| RSS after the workload (MiB) | 85.2 (81.9–90.5) | 91.4 (88.5–116.1) | 0.93x | 84.0 (83.1–85.4) | |
| Idle CPU in the 30 s after the workload (% of a core) | 6.5 (5.2–7.4) | 10.2 (6.9–17.0) | 0.64x | 10.2 (9.9–11.0) | 0.64x |
| Wakeups in the 30 s after the workload (per s) | 25.0 (24.0–25.4) | 27.4 (27.0–28.7) | 0.91x | 23.5 (22.8–24.2) | |
| Footprint after those 30 s (MiB) | 35.1 (33.6–41.4) | 41.7 (33.6–62.8) | 0.84x | 35.4 (29.3–51.8) | |

| Operation, ms (median of per-trial p50 / p95) | Godspeed p50 | Godspeed p95 | Zeron p50 | Zeron p95 | G/Z p50 | 0.2.0 p50 |
|---|---:|---:|---:|---:|---:|---:|
| `list_empty` | 0.113 | 0.147 | 0.115 | 0.154 | 0.98x | 0.136 |
| `create_thread` | 0.204 | 0.360 | 0.201 | 0.364 | 1.01x | 0.192 |
| `list_1p_200t` | 0.676 (0.663–0.730) | 0.724 | 0.593 (0.574–1.513) | 0.651 | **1.14x** | 0.684 |
| `create_project_thread` | 1.441 | 2.107 | 1.458 | 2.106 | 0.99x | 1.452 |
| `list_201p_400t` (50 reps) | 1.329 (1.308–1.397) | 1.483 | 1.118 (1.092–2.921) | 1.233 | **1.19x** | 1.310 |

The list gap with Zeron is consistent: in 5 of 6 trials, Zeron's per-trial
p50 sits below Godspeed's minimum. Godspeed 0.2.0 matches 0.5.9, so the gap
comes from Zeron getting faster upstream.

### Streaming (fake Claude CLI, 5,000-delta turns)

| Metric | Godspeed 0.5.9 | Zeron 0.2.130 | G / Z | Godspeed 0.2.0 | 0.5.9 / 0.2.0 |
|---|---:|---:|---:|---:|---:|
| **Paced** (5,000 deltas, 1 ms apart): send → done (ms) | 6,696 (6,585–7,931) | 7,277 (6,501–7,978) | 0.92x | 7,896 (6,477–7,983) | |
| last delta emitted → done (ms) | 6.5 (2.9–11.3) | 5.3 (4.5–11.4) | | 6.3 (2.5–10.0) | |
| per-delta emit → visible, p50 (ms) | 62.3 (61.0–63.5) | 62.3 (61.6–63.3) | 1.00x | 62.2 | |
| per-delta emit → visible, p95 (ms) | 116.9 (116.5–119.0) | 117.2 (116.4–118.3) | 1.00x | 117.0 | |
| engine CPU, send → done + 2 s (ms) | 400 (297–502) | 385 (192–508) | 1.04x | 426 (133–516) | 0.94x |
| engine wakeups, same span | 444 (432–455) | 434 (423–479) | 1.02x | 430 | |
| frames / bytes on the wire | 56 / 874 KiB | 61 / 874 KiB | | 65 / 873 KiB | |
| engine footprint growth (MiB) | 30.2 (18.8–36.2) | 32.0 (26.1–34.7) | 0.95x | 36.9 (24.9–38.6) | 0.82x |
| **Burst** (5,000 deltas, no pacing): send → done (ms) | 161 (159–207) | 123 (113–276) | 1.31x | 179 (173–435) | 0.90x |
| … first delta emitted → done (ms) | 22.9 (22.5–31.6) | 22.7 (22.3–79.8) | 1.01x | 22.7 | |
| … send → agent emitting (spawn and dispatch, ms) | 136 | 96 | 1.42x | 151 | 0.90x |
| … engine CPU (ms) | 55 (48–64) | 44 (40–74) | **1.25x** | 51 (49–75) | 1.09x |
| **Follow-up turns 2 → 10** on the burst chat: send → done, turn 2 (ms) | **41.3 (40.2–46.4)** | 57.3 (52.5–155.0) | **0.72x** | 53.9 | 0.77x |
| … send → done, turn 10 (ms) | **45.8 (43.5–57.3)** | 65.7 (60.3–100.1) | **0.70x** | 59.7 | 0.77x |
| … send → agent emitting, median turn (ms) | 17.5 | 30.7 | 0.57x | 28.6 | |
| … engine CPU, turn 2 / turn 10 (ms) | 56.8 / 84.9 | 44.5 / 67.4 | 1.28x / 1.26x | 52.8 / 79.8 | |
| … engine CPU, turns 2–10 total (ms) | 658 (529–700) | 528 (436–764) | **1.25x** | 611 (482–714) | 1.08x |
| Engine footprint before any stream (MiB) | 12.2 | 11.7 | 1.04x | 11.8 | |
| Engine footprint after all 11 turns (MiB) | **78.9 (70.1–116.8)** | 100.6 (75.0–134.4) | **0.78x** | 100.1 (87.6–122.5) | 0.79x |
| Engine peak footprint during the streams (MiB) | 96.9 (80.1–128.6) | 108.1 (83.6–146.4) | 0.90x | 108.3 | |
| Data dir after the streams (KiB) | 13,919 | 13,680 | 1.02x | 13,959 | |

Median send → done (ms) and engine CPU (ms) for each follow-up turn:

| Turn | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Godspeed 0.5.9, send → done | 41.3 | 45.1 | 43.3 | 43.9 | 44.3 | 43.9 | 44.5 | 45.7 | 45.8 |
| Zeron 0.2.130, send → done | 57.3 | 60.2 | 57.7 | 60.1 | 62.1 | 59.6 | 62.8 | 61.4 | 65.7 |
| Godspeed 0.5.9, engine CPU | 56.8 | 74.0 | 64.0 | 68.8 | 74.9 | 72.7 | 84.6 | 79.3 | 84.9 |
| Zeron 0.2.130, engine CPU | 44.5 | 55.2 | 53.5 | 53.2 | 59.8 | 64.8 | 68.1 | 68.4 | 67.4 |

Notes on streaming:

- **Delta latency is set by the fold cadence, not the engine's speed.**
  Every build folds agent events into the chat doc on a 120 ms tick, so a
  delta waits 0–120 ms (p50 62 ms, p95 117 ms) whatever the build. The wire
  traffic is the same in all three (about 874 KiB for 436 KB of text). The
  text-append path sends only new text.
- **Per-turn CPU grows with transcript length in both apps**, by about 3 ms
  per 5,000-delta turn already in the chat. Godspeed's slope matches
  Zeron's; its whole curve sits about 25% higher.
- **Godspeed is quicker on everything after the first turn.** Its follow-up
  dispatch is 17.5 ms against Zeron's 30.7 ms, so its turns finish about
  15–20 ms sooner even though it spends more CPU on them. The first turn on
  a new chat is the other way round. Most of the time goes to spawning the
  agent, which includes about 80 ms of Python start-up in the fake for both
  apps. Godspeed takes about 40 ms longer there, and 0.2.0 was 15 ms slower
  still, so Zeron sped this path up upstream.
- **A streamed turn's memory cost is about 70x its text, in every build.**
  The footprint grows about 30 MiB for 436 KB of streamed text, and only
  part of that comes back.

### Long transcript: reads and restart

| Metric | Godspeed 0.5.9 | Zeron 0.2.130 | G / Z | Godspeed 0.2.0 |
|---|---:|---:|---:|---:|
| `WatchDocMessages` initial reset, 4.3 MB transcript, warm, p50 of 10 (ms) | 7.8 (7.6–8.6) | 8.4 (8.1–13.4) | 0.93x | 7.9 |
| `StopEngine` → exit with a finished agent process (ms) | 4,628 (4,588–4,709) | 4,613 (4,566–4,630) | 1.00x | 4,658 |
| Restart on the populated profile → `EngineReady` (ms) | 54 (50–75) | 59 (54–68) | 0.92x | 51 |
| First (cold) read after restart (ms) | 17.1 (16.7–19.6) | 17.8 (17.0–20.9) | 0.96x | 17.5 |
| Reads 2–11 after restart, p50 (ms) | 8.7 (8.1–11.6) | 8.7 (8.2–10.4) | 1.00x | 8.6 |
| **Engine footprint after the restart and 11 reads (MiB)** | **111.1 (93.1–113.0)** | **75.5 (58.4–92.4)** | **1.47x** | 109.0 (95.1–133.8) |

Shutdown with a populated profile takes about 4.6 s in all three. The
equality and fixed size suggest a fixed grace period for the finished agent
process, rather than work.

The post-restart memory gap is the largest relative gap in this run. Reading
a 4.3 MB transcript 11 times leaves the Godspeed engine 36 MiB larger than
Zeron's. 0.2.0 shows the same figure.

### Static footprint

| | Godspeed 0.5.9 | Zeron 0.2.130 | Godspeed 0.2.0 |
|---|---:|---:|---:|
| App bundle on disk (`du -sk`) | 158.8 MiB (162,620 KiB) | 205.4 MiB (210,312 KiB) | 143.8 MiB (147,268 KiB) |
| Sum of file sizes | 166,501,382 B | 215,333,909 B | 150,778,094 B |
| Main executable | 164,289,328 B | 157,781,440 B (+ `Zeron Engine` 55,000,256 B) | 148,005,680 B |
| Release archive (`*-macos-arm64-app.tar.gz`) | 60,169,224 B | 80,037,016 B | 52,482,785 B |

The release executables are stripped (about 1,200 symbols), so `cargo bloat`
cannot attribute size to crates. This breakdown comes from `size -m`
sections, in bytes; `__const` sums `__TEXT` and `__DATA_CONST`:

| Section | 0.2.0 | 0.5.9 | Zeron 0.2.130 | 0.5.9 − 0.2.0 | 0.5.9 − Zeron |
|---|---:|---:|---:|---:|---:|
| `__text` (code) | 87,406,884 | 95,611,556 | 92,440,892 | +8,204,672 (+9.4%) | +3,170,664 |
| `__const` (tables, embedded assets) | 41,187,592 | 47,291,848 | 44,495,816 | +6,104,256 (+14.8%) | +2,796,032 |
| `__eh_frame` | 8,622,764 | 9,625,940 | 9,211,292 | +1,003,176 | +414,648 |
| `__gcc_except_tab` | 5,180,716 | 5,763,356 | 5,562,108 | +582,640 | +201,248 |
| `__unwind_info` | 1,742,560 | 1,920,600 | 1,851,704 | +178,040 | +68,896 |
| `__cstring` | 1,730,054 | 1,763,102 | 1,735,794 | +33,048 | +27,308 |

Since 0.2.0, the main executable grew 16.3 MB (+11%). About half of that is
code and about 40% is `__const`, which includes embedded assets such as
fonts, icons and syntax grammars. The rest is unwind metadata, which grows
with the code. Against Zeron's main executable, Godspeed's is 6.5 MB (4%)
larger. Zeron's whole bundle is still 47 MiB larger, because it adds the
separate `Zeron Engine`.

## Where the idle cost comes from

The per-second CPU timeline is the median of 6 trials, over + 11 → + 130 s.
The steady baseline, in ms of CPU per second, is 0.80 for Godspeed 0.5.9,
0.63 for Zeron and 0.73 for 0.2.0. On top of that baseline:

| When | Godspeed 0.5.9 | Zeron 0.2.130 | Godspeed 0.2.0 | What it is |
|---|---:|---:|---:|---|
| + 20–21 s | +7.8 ms | +10.8 ms | +2.9 ms | Update check (`ENGINE_INITIAL_DELAY = 20 s` in all three) |
| + 60–61 s | +7.4 ms | +1.0 ms | +0.2 ms | Godspeed's telemetry ping (`LAUNCH_DELAY = 60 s`) |
| + 81 s | — | — | +5 ms | unidentified (0.2.0 only) |

Network evidence:

- **lsof, Godspeed 0.5.9.** In all 6 trials, a TLS connection to
  `github.com` (`20.207.73.82:443`) was open at + 20 s. That is the update
  check fetching
  `github.com/ShriD5/godspeed-code/releases/latest/download/manifest.json`.
  It was gone by + 60 s. A TLS connection to a Cloudflare address opened at
  + 60 s: `2a06:98c1:312x::3` in 2 trials and `2606:4700:…` in 4, both of
  which are Cloudflare anycast. That is the daily usage ping to
  `godspeed-edge.shrid5.workers.dev`. It stayed ESTABLISHED (pooled) through
  + 125 s and the workload in every trial.
- **lsof, Zeron 0.2.130.** At + 20 s, Zeron dialed `edge.zeron.sh` over IPv6,
  and the socket was still in `SYN_SENT` in 6 of 6 trials. Its update check
  probably never finished on this network, so its idle CPU here is if
  anything **understated**. No other external socket was seen.
- **lsof, Godspeed 0.2.0.** No external socket: its default edge was
  `edge.godspeed.invalid`.
- **Debug logs (`attrib.py`, `RUST_LOG=debug`, one run each).** Past + 15 s,
  the only log lines from either idle engine are the HTTP connects at
  + 20.1 s (both apps) and + 60.1 s (Godspeed). The steady baseline logs
  nothing. It is timers that wake, find nothing to do, and sleep again.
- **Telemetry off.** With `GODSPEED_TELEMETRY=0`, the + 60 s connection
  disappears. Wakeups stay at 22.7 /s, so the higher wakeup rate is not
  network-related.

The wakeup timeline points at a 4 s timer. Both Godspeed builds show extra
wakeups every 4 s (a 23–27/s bump every fourth second), while Zeron's
timeline is flat. The best match in the source is the Telegram delivery
loop (`crates/engine/src/telegram/delivery.rs`). It wakes every
`TICK = 4 s`, plus a 100 ms coalesce sleep, even when no bot token is
configured, because `self.api()` is checked inside the loop. This is an
inference from the timeline and the code, not a profile.

Other Godspeed-only periodic jobs could add to the baseline: `pr_watch`
(15 s loop tick), `build_slots` (30 s tick), `change_requests` (15 s) and
the agent scheduler. Some are probably gated off with no projects, no
GitHub remote and no credentials, which would explain why they don't
stand out. **They were not exercised here**: no GitHub remote, no `gh`
login, no scheduled agents, no Telegram token and no provider credentials.
A real profile with open PRs and agent accounts may cost more. See
[Limitations](#limitations).

## GUI: not measured

The GUI was skipped on purpose. Both apps honor a data-dir override in
headed mode: `GODSPEED_DATA_DIR` (`apps/godspeed/src/paths.rs`) and
`ZERON_DATA_DIR`. `open -g -j -n --env …` could pass it. But both GPUI apps
call `cx.activate(true)` at launch: `crates/ui/src/lib.rs` lines 328 and 334
in 0.5.9, and lines 319 and 325 in Zeron 0.2.130. That activates the app
whatever `open -g` asks for, and the brief allows zero focus stealing. A
fresh profile would also open onboarding and could trigger notification or
keychain prompts. So no GUI process was launched, and no screenshot was
taken.

## Raw data

CSV files are committed next to this doc:

- [`bench-0.5.9/results.csv`](bench-0.5.9/results.csv): one row per kept
  trial, all metrics.
- [`bench-0.5.9/coldstart.csv`](bench-0.5.9/coldstart.csv): the
  back-to-back cold-start series.

Everything else is in `/Volumes/Lexar/tmp/perf-bench/`, on the owner's
external drive:

- `raw/results.jsonl`: all 18 scheduled trials, including the excluded
  Godspeed trial 6, with the per-second timelines, per-turn records, the
  fake agent's emit timestamps, socket samples and warning log lines.
- `raw/results_extra.jsonl`: the replacement trial.
- `raw/results_final.jsonl`: the set the tables use.
- `raw/summary.json`, `raw/analysis_final.txt`, `raw/run_all.log`,
  `raw/followup.log`, `raw/coldstart.jsonl` and `raw/attrib_*.json`.
- `raw/pilot.jsonl`: short pilot runs used to debug the harness; not part of
  the results.
- `scripts/`: `bench.py`, `run_all.py`, `analyze.py`, `pb.py` (rusage, IPC
  client, streaming), `fake_claude.py`, `coldstart.py`, `attrib.py`,
  `followup.sh` and `rawws.py`.
- `godspeed-dl/`, `zeron-dl/`: the untouched downloads and their extracted
  bundles.

Compact per-trial CSV (kept trials, in run order):

```csv
app,trial,started_at,loadavg_1m,cold_start_ready_ms,footprint_30s_mib,idle_cpu_ms_120s,idle_wakeups_per_s,idle_minstr_120s,workload_cpu_ms,footprint_post_ops_mib,list_201p_400t_p50_ms,paced_engine_cpu_ms,burst_engine_cpu_ms,turns2_10_engine_cpu_ms_total,turn10_send_to_done_ms,engine_footprint_after_streams_mib,transcript_read_cold_ms,engine_footprint_after_restart_read_mib
g059,1,2026-10-30T14:04:12+0530,6.95,61.94,12.75,131.00,22.59,112.53,3443.65,34.84,1.31,463.59,47.96,690.51,43.60,76.80,16.90,112.22
z130,1,2026-10-30T14:07:45+0530,6.86,232.21,11.16,95.17,21.57,79.91,6077.27,47.74,1.15,368.77,46.63,540.87,63.13,75.02,17.91,92.38
g020,1,2026-10-30T14:11:20+0530,6.14,1443.86,12.25,114.76,20.72,65.74,6134.17,36.69,1.29,448.57,54.30,632.07,60.48,101.69,17.89,117.74
g020,2,2026-10-30T14:14:56+0530,6.60,62.84,12.41,117.09,20.74,63.41,6312.01,41.42,1.30,516.25,50.01,608.80,57.31,122.50,17.10,95.06
z130,2,2026-10-30T14:18:31+0530,5.78,256.08,11.22,125.91,21.57,79.95,6185.46,46.52,1.12,433.63,43.89,514.85,100.14,134.39,18.39,74.80
g059,2,2026-10-30T14:22:05+0530,8.57,1606.34,13.27,118.31,22.59,118.10,3346.01,34.52,1.31,296.91,63.73,622.89,57.32,116.84,18.56,112.99
g059,3,2026-10-30T14:25:39+0530,5.00,77.79,13.22,90.05,22.54,113.24,3433.36,32.89,1.33,376.40,53.37,626.17,49.32,79.70,19.56,95.58
z130,3,2026-10-30T14:29:13+0530,6.47,1891.53,10.75,78.96,21.82,75.54,6160.37,47.19,1.09,382.44,40.41,477.39,67.24,108.47,17.54,59.64
g020,3,2026-10-30T14:32:48+0530,7.26,1401.87,12.49,108.81,20.92,65.31,6229.60,38.58,1.31,429.76,49.90,612.76,56.26,100.22,16.94,101.69
g020,4,2026-10-30T14:36:24+0530,3.64,71.11,11.61,105.65,20.44,62.46,6193.26,35.72,1.32,422.64,52.10,585.26,59.00,87.59,17.01,116.39
z130,4,2026-10-30T14:39:59+0530,2.06,274.31,11.27,124.18,21.15,77.78,6297.35,52.00,1.11,387.45,44.75,583.20,60.29,92.70,17.62,58.44
g059,4,2026-10-30T14:43:33+0530,1.76,277.75,12.80,142.39,22.12,118.71,3418.80,42.17,1.33,502.05,59.63,700.01,45.98,78.11,17.30,93.06
g059,5,2026-10-30T14:47:09+0530,1.37,87.29,12.75,122.82,22.34,110.40,3677.80,40.99,1.40,416.88,50.20,529.04,45.66,70.08,16.82,110.33
z130,5,2026-10-30T14:50:42+0530,2.23,241.69,11.31,65.44,22.07,82.42,6394.62,38.80,1.12,191.99,39.65,435.83,64.10,88.97,17.02,92.42
g020,5,2026-10-30T14:54:15+0530,7.04,268.28,11.97,62.37,21.11,66.98,6359.73,34.92,1.34,202.14,49.09,482.45,63.08,99.84,17.88,133.77
g020,6,2026-10-30T14:57:48+0530,2.41,74.64,12.02,75.00,20.78,62.77,6260.27,33.91,1.31,132.75,75.22,714.16,194.03,100.00,26.07,100.19
z130,6,2026-10-30T15:03:24+0530,8.44,318.57,11.08,69.47,21.85,86.90,8466.75,50.20,2.92,508.34,74.14,764.14,80.23,130.77,20.90,76.17
g059,7,2026-10-30T16:21:15+0530,2.60,1692.00,13.19,80.94,22.42,114.38,3334.04,43.03,1.34,383.79,57.52,695.50,43.46,99.05,16.71,111.81
```

## Limitations

- **Headless only.** There is no GUI number at all (see above). For both
  apps, rendering, window memory and the GUI's own timers are unmeasured.
- **No real agent, and one harness.** The fake Claude CLI exercises each
  engine's real Claude driver: process spawn, stream-json parsing, the fold,
  doc commits and the IPC fan-out. It costs nothing and starts in about
  80 ms. A real CLI takes seconds to start and streams far slower than 1
  delta/ms. Codex, ACP agents and the mock harness were not streamed.
- **Real-world idle features were dormant.** With no GitHub remote, `gh`
  auth, scheduled agents, Telegram bot, provider accounts or usage polling,
  the periodic work these features add was not exercised. That work is the
  most likely place for a larger idle cost. This run cannot rule it out.
- **Network differs by app.** Both ran as shipped. Godspeed's update check
  and telemetry reached their servers. Zeron's update check never got past
  `SYN_SENT` over IPv6. The idle CPU comparison therefore leans slightly in
  Zeron's favor.
- **Noisy machine.** Load was 1.4–8.6, with the owner's apps, Spotlight and
  other agents running. Instructions retired and footprint were tight across
  trials. CPU time (ms), cold start, p95 tails and the first-turn spawn
  times were visibly noisy. Treat differences under about 15% in those as
  ties.
- **Cold start is bimodal.** Every build had launches of 1.4–1.9 s against
  60–300 ms for the rest. The slow launches burned about 2.5x the CPU before
  ready, with no page-ins, which looks like the system's launch-time code
  assessment. They were not isolated, and they hit all builds. The binaries
  ran from a USB SSD, unlike the internal-SSD copies of the 0.2.0 run, so
  absolute start times here cannot be compared with the 65 ms in the older
  doc.
- **Workload CPU includes children.** The tree's CPU covers reaped `git`
  children. Which `git` calls make Zeron's (and 0.2.0's) workload cost 2.8 s
  more was not broken down.
- **Attribution is inference.** The 4 s Telegram timer and the network jobs
  are matched by timing and source reading, not by a profiler. `sample` and
  `spindump` were not used.
- **Sample size.** There are 6 trials per build, and the cold-start series
  has 10 per build. Ranges are reported instead of confidence intervals.
